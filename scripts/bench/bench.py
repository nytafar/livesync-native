#!/usr/bin/env python3
"""lsync vs the official LiveSync CLI vs the official plug-in in Obsidian.

Run through scripts/bench/run.sh (it sources lib.sh for the dev credentials).

Every repetition of every contender starts from a fresh clone of the snapshot
database ($SNAP_DB) into that contender's own database (bench-lsync,
bench-official, bench-plugin), and runs the same sequence on it:

  1. cold     first sync into an empty folder (measured)
  2. writer   a separate lsync profile, scoped to bench-writes/, joins the database
  3. inc-10   the client is stopped; the writer pushes 10 new files; the client's
              daemon starts; time until all 10 are in its vault
  4. inc-1000 the same with 1,000 files
  5. idle     the daemon keeps running with no changes (settle, then measure)
  6. live     server->local: the writer pushes one file, time from the moment the
              server's _changes feed shows it to the file appearing in the vault;
              local->server: a file written into the vault, time until _changes
              shows its document
Then the daemon stops. Results are appended as JSON lines to results.jsonl.
"""
import argparse
import base64
import hashlib
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from compare_trees import digest_tree  # noqa: E402

E = os.environ
URL, USER, PASSWORD = E["LSYNC_URL"].rstrip("/"), E["LSYNC_USER"], E["LSYNC_PASSWORD"]
assert URL.startswith(("http://127.0.0.1:8411", "http://localhost:8411")), "dev container only"
BIN, CLI_JS = E["BIN"], E["CLI_JS"]
WORK = E["BENCH_WORK"]
REF = os.path.abspath(os.path.join(os.path.dirname(CLI_JS), "../../../.."))
SCOPE_EXCLUDE, SCOPE_RE, BASE_RE = E["SCOPE_EXCLUDE"], E["SCOPE_IGNORE_REGEX"], E["BASE_IGNORE_REGEX"]
CONTAINER = E.get("DEV_CONTAINER", "lsync-dev-couchdb")
AUTH = "Basic " + base64.b64encode(f"{USER}:{PASSWORD}".encode()).decode()
CLK = os.sysconf("SC_CLK_TCK")
PAGE = os.sysconf("SC_PAGE_SIZE")
RESULTS = os.path.join(WORK, "results.jsonl")
# The extracted Obsidian AppImage (see run.sh).
OBSIDIAN_DIR = E.get("OBSIDIAN_DIR", os.path.expanduser("~/scratch/lsync-bench/obsidian"))


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, file=sys.stderr, flush=True)


def record(rec):
    rec["ts"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    with open(RESULTS, "a") as f:
        f.write(json.dumps(rec) + "\n")
    log("RESULT", json.dumps(rec))


def couch(path, method="GET", body=None):
    req = urllib.request.Request(URL + path, method=method, data=None if body is None else json.dumps(body).encode(),
                                 headers={"Authorization": AUTH, "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=600) as r:
        return json.load(r)


def clone(db):
    t = time.monotonic()
    subprocess.run(["bash", "-c", f'source "{HERE}/lib.sh" && clone_db "$1"', "_", db], check=True)
    log(f"cloned {E['SNAP_DB']} -> {db} in {time.monotonic() - t:.0f}s")


# --- container-wide counters ------------------------------------------------------
def net():
    out = subprocess.run(["docker", "exec", CONTAINER, "cat", "/sys/class/net/eth0/statistics/tx_bytes",
                          "/sys/class/net/eth0/statistics/rx_bytes"], capture_output=True, text=True,
                         check=True).stdout.split()
    return int(out[0]), int(out[1])


_cid = None


def server_cpu():
    global _cid
    _cid = _cid or subprocess.run(["docker", "inspect", "-f", "{{.Id}}", CONTAINER], capture_output=True, text=True,
                                  check=True).stdout.strip()
    for f in (f"/sys/fs/cgroup/system.slice/docker-{_cid}.scope/cpu.stat", f"/sys/fs/cgroup/docker/{_cid}/cpu.stat"):
        if os.path.exists(f):
            for line in open(f):
                k, v = line.split()
                if k == "usage_usec":
                    return int(v) / 1e6
    return float("nan")


class Counters:
    def __init__(self):
        self.n0, self.c0 = net(), server_cpu()

    def done(self):
        n1, c1 = net(), server_cpu()
        return {"net_down_mb": round((n1[0] - self.n0[0]) / 1e6, 2), "net_up_mb": round((n1[1] - self.n0[1]) / 1e6, 2),
                "server_cpu_s": round(c1 - self.c0, 2)}


# --- process-tree sampling ----------------------------------------------------------
def children_map():
    m = {}
    for d in os.listdir("/proc"):
        if d.isdigit():
            try:
                with open(f"/proc/{d}/stat") as f:
                    s = f.read()
                ppid = int(s[s.rindex(")") + 2:].split()[1])
                m.setdefault(ppid, []).append(int(d))
            except (FileNotFoundError, ProcessLookupError, ValueError):
                pass
    return m


def tree(pid):
    m, out, todo = children_map(), [], [pid]
    while todo:
        p = todo.pop()
        out.append(p)
        todo.extend(m.get(p, []))
    return out


def proc_stats(pids):
    """CPU seconds and RSS/PSS bytes summed over `pids`, and context switches per thread id."""
    cpu = rss = pss = wbytes = 0
    ctx = {}
    for p in pids:
        try:
            with open(f"/proc/{p}/stat") as f:
                s = f.read()
            fields = s[s.rindex(")") + 2:].split()
            cpu += (int(fields[11]) + int(fields[12])) / CLK
            rss += int(fields[21]) * PAGE
            with open(f"/proc/{p}/io") as f:
                for line in f:
                    if line.startswith("write_bytes:"):
                        wbytes += int(line.split()[1])
            with open(f"/proc/{p}/smaps_rollup") as f:
                for line in f:
                    if line.startswith("Pss:"):
                        pss += int(line.split()[1]) * 1024
            for t in os.listdir(f"/proc/{p}/task"):
                try:
                    with open(f"/proc/{p}/task/{t}/status") as f:
                        n = 0
                        for line in f:
                            if line.startswith(("voluntary_ctxt_switches", "nonvoluntary_ctxt_switches")):
                                n += int(line.split()[1])
                        ctx[int(t)] = n
                except FileNotFoundError:
                    pass
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            pass
    return {"cpu": cpu, "rss": rss, "pss": pss, "ctx": ctx, "wbytes": wbytes}


def ctx_delta(first, last):
    """Context switches between two samples: per thread, threads born in between count in full
    (threads that ended in between are lost, so this is a lower bound)."""
    return sum(n - first.get(t, 0) for t, n in last.items() if n >= first.get(t, 0))


class Sampler(threading.Thread):
    """Samples a process tree every `every` s: CPU (sum over live processes), RSS and PSS sums."""

    def __init__(self, pid, every=1.0):
        super().__init__(daemon=True)
        self.pid, self.every, self.samples, self.stop_ev = pid, every, [], threading.Event()
        self.first = proc_stats(tree(pid))

    def run(self):
        while not self.stop_ev.is_set():
            self.samples.append(proc_stats(tree(self.pid)))
            self.stop_ev.wait(self.every)

    def done(self):
        self.stop_ev.set()
        self.join()
        last = proc_stats(tree(self.pid))
        self.samples.append(last)
        rss = [s["rss"] for s in self.samples]
        pss = [s["pss"] for s in self.samples]
        return {"cpu_s": round(last["cpu"] - self.first["cpu"], 2),
                "peak_rss_mb": round(max(rss) / 1e6, 1), "mean_rss_mb": round(statistics.mean(rss) / 1e6, 1),
                "peak_pss_mb": round(max(pss) / 1e6, 1), "mean_pss_mb": round(statistics.mean(pss) / 1e6, 1),
                "ctx_switches": ctx_delta(self.first["ctx"], last["ctx"]),
                # Bytes the tree caused to be written to storage (/proc/<pid>/io write_bytes).
                "disk_write_mb": round((last["wbytes"] - self.first["wbytes"]) / 1e6, 1), "processes": len(tree(self.pid))}


# --- the _changes feed -----------------------------------------------------------------
class Follower(threading.Thread):
    """Follows <db>/_changes from now and remembers when each document id first showed up."""

    def __init__(self, db):
        super().__init__(daemon=True)
        self.db, self.seen, self.ready = db, {}, threading.Event()
        self.resp = None

    def run(self):
        req = urllib.request.Request(f"{URL}/{self.db}/_changes?feed=continuous&since=now&heartbeat=2000",
                                     headers={"Authorization": AUTH})
        self.resp = urllib.request.urlopen(req, timeout=60)
        self.ready.set()
        try:
            self._follow()
        except Exception:  # closed by close(), or the connection dropped
            pass

    def _follow(self):
        for line in self.resp:
            line = line.strip()
            if not line:
                continue
            t = time.monotonic()
            try:
                d = json.loads(line)
            except ValueError:
                continue
            if "id" in d:
                self.seen.setdefault(d["id"], t)

    def start(self):
        super().start()
        self.ready.wait(30)

    def close(self):
        try:
            self.resp.close()
        except Exception:
            pass

    def wait_for(self, doc_id, timeout):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if doc_id in self.seen:
                return self.seen[doc_id]
            time.sleep(0.005)
        return None


def wait_files(paths, timeout, poll=0.01):
    """Wait until every (path, content) exists with that content. Returns monotonic time or None."""
    pending = dict(paths)
    end = time.monotonic() + timeout
    while pending and time.monotonic() < end:
        for p, want in list(pending.items()):
            try:
                with open(p, "rb") as f:
                    if f.read() == want:
                        del pending[p]
            except (FileNotFoundError, IsADirectoryError, PermissionError):
                pass
        if pending:
            time.sleep(poll)
    return None if pending else time.monotonic()


def du(path):
    total = 0
    for root, _d, files in os.walk(path):
        for n in files:
            try:
                total += os.lstat(os.path.join(root, n)).st_blocks * 512
            except FileNotFoundError:
                pass
    return total


def vault_stats(path):
    n = size = 0
    for root, dirs, files in os.walk(path):
        if root == path:
            dirs[:] = [d for d in dirs if d != ".obsidian"]
        for f in files:
            p = os.path.join(root, f)
            if os.path.isfile(p) and not os.path.islink(p):
                n += 1
                size += os.path.getsize(p)
    return n, size


def note_body(tag, i):
    # About 1.2 KB of markdown, distinct per file.
    line = f"- item {i} of {tag}: the quick brown fox jumps over the lazy dog æøå\n"
    return (f"# {tag} {i}\n\n" + line * 16).encode()


# --- the writer: a separate lsync profile scoped to bench-writes/ -----------------------
class Writer:
    def __init__(self, db, root):
        self.db, self.root = db, root
        self.vault = os.path.join(root, "vault")
        self.cfg = os.path.join(root, "writer.toml")
        os.makedirs(self.vault, exist_ok=True)
        self.env = lsync_env()
        self.run("configure", "--url", URL, "--db", db, "--user", USER, "--vault", self.vault,
                 "--state-dir", os.path.join(root, "state"), "--include", "bench-writes",
                 "--ignore-regex", BASE_RE, "--device-name", "bench-writer", "--password-stdin", stdin=PASSWORD)

    def run(self, *args, stdin=None):
        with open(os.path.join(self.root, "writer.log"), "ab") as lf:
            rc = subprocess.run([BIN, "--config", self.cfg, *args], env=self.env, input=stdin and stdin.encode(),
                                stdout=lf, stderr=lf).returncode
        if rc not in (0, 3):
            raise RuntimeError(f"writer {args} exited {rc}")

    def sync(self):
        self.run("sync")

    def put(self, rel, body):
        p = os.path.join(self.vault, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "wb") as f:
            f.write(body)


def lsync_env():
    env = {k: v for k, v in E.items() if not k.startswith("LSYNC_")}
    env["RUST_LOG"] = "info"
    return env


# --- contenders ---------------------------------------------------------------------------
class Lsync:
    name, db = "lsync", "bench-lsync"

    def __init__(self, rep, resume=False):
        assert not resume, "resume is implemented for the official CLI only"
        self.root = os.path.join(WORK, "lsync", f"r{rep}")
        shutil.rmtree(self.root, ignore_errors=True)
        self.vault, self.state = os.path.join(self.root, "vault"), os.path.join(self.root, "state")
        os.makedirs(self.vault)
        subprocess.run([BIN, "--profile", "bench", "configure", "--url", URL, "--db", self.db, "--user", USER,
                        "--vault", self.vault, "--state-dir", self.state, "--exclude", SCOPE_EXCLUDE,
                        "--ignore-regex", BASE_RE, "--device-name", "bench-lsync", "--password-stdin"],
                       env=lsync_env(), input=PASSWORD.encode(), check=True, capture_output=True)
        self.pid = None

    def cold(self):
        out = subprocess.run([sys.executable, os.path.join(HERE, "measure.py"), "--label", "cold", "--state",
                              self.state, "--vault", self.vault, "--log", os.path.join(self.root, "cold.log"), "--stamp", "--",
                              BIN, "--profile", "bench", "sync", "--json"], env=lsync_env(), capture_output=True,
                             text=True, check=True).stdout
        m = json.loads(out.strip().splitlines()[-1])
        return {"wall_s": m["wall_s"], "cpu_s": m["cpu_s"], "peak_rss_mb": m["maxrss_mb"],
                "net_down_mb": m["net_tx_mb"], "net_up_mb": m["net_rx_mb"], "server_cpu_s": m["server_cpu_s"],
                "state_mb": m["state_mb"], "rc": m["rc"], "disk_write_mb": m.get("disk_write_mb")}

    def start(self):
        subprocess.run(["systemctl", "--user", "start", "lsync-bench.service"], check=True)
        for _ in range(100):
            pid = int(subprocess.run(["systemctl", "--user", "show", "-p", "MainPID", "--value", "lsync-bench.service"],
                                     capture_output=True, text=True).stdout.strip() or 0)
            if pid:
                self.pid = pid
                return pid
            time.sleep(0.05)
        raise RuntimeError("lsync-bench did not start")

    def stop(self):
        subprocess.run(["systemctl", "--user", "stop", "lsync-bench.service"], check=True)
        self.pid = None

    def local_write(self, rel, body):
        p = os.path.join(self.vault, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "wb") as f:
            f.write(body)

    def state_mb(self):
        return round(du(self.state) / 1e6, 1)

    def close(self):
        if self.pid:
            self.stop()


class Official:
    name, db = "official", "bench-official"

    def __init__(self, rep, resume=False):
        self.root = os.path.join(WORK, "official", f"r{rep}")
        self.vault, self.dbdir = os.path.join(self.root, "vault"), os.path.join(self.root, "db")
        self.settings = os.path.join(self.root, "cli.json")
        self.proc = self.pid = None
        if resume:
            return
        shutil.rmtree(self.root, ignore_errors=True)
        os.makedirs(self.vault)
        os.makedirs(self.dbdir)
        subprocess.run(["bash", "-c", f'source "{HERE}/lib.sh" && cli_settings "$1" "" "$2"', "_", self.db,
                        self.settings], check=True)
        self.edit_settings(customChunkSize=60)
        self.proc = None
        self.pid = None

    def edit_settings(self, **kv):
        d = json.load(open(self.settings))
        d.update(kv)
        fd = os.open(self.settings, os.O_WRONLY | os.O_TRUNC | os.O_CREAT, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump(d, f, indent=2)
        os.chmod(self.settings, 0o600)

    def cli(self, *args):
        return ["node", CLI_JS, self.dbdir, "--settings", self.settings, "--vault", self.vault, *args]

    def cold(self):
        out = subprocess.run([sys.executable, os.path.join(HERE, "measure.py"), "--label", "cold", "--state",
                              self.dbdir, "--vault", self.vault, "--log", os.path.join(self.root, "cold.log"), "--stamp", "--",
                              *self.cli("fetch", "--force", "--write-settings")], capture_output=True, text=True,
                             check=True).stdout
        os.chmod(self.settings, 0o600)  # --write-settings recreates it with the default mode
        m = json.loads(out.strip().splitlines()[-1])
        return {"wall_s": m["wall_s"], "cpu_s": m["cpu_s"], "peak_rss_mb": m["maxrss_mb"],
                "net_down_mb": m["net_tx_mb"], "net_up_mb": m["net_rx_mb"], "server_cpu_s": m["server_cpu_s"],
                "state_mb": m["state_mb"], "rc": m["rc"], "disk_write_mb": m.get("disk_write_mb")}

    def start(self):
        self.edit_settings(liveSync=True)
        lf = open(os.path.join(self.root, "daemon.log"), "ab")
        self.proc = subprocess.Popen(self.cli("daemon"), stdout=lf, stderr=lf, start_new_session=True)
        self.pid = self.proc.pid
        return self.pid

    def stop(self):
        if self.proc:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(60)
            except subprocess.TimeoutExpired:
                os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait()
        self.proc = self.pid = None

    local_write = Lsync.local_write

    def state_mb(self):
        return round(du(self.dbdir) / 1e6, 1)

    def close(self):
        self.stop()
        if os.path.exists(self.settings):
            os.chmod(self.settings, 0o600)


class Plugin:
    """The real plug-in in a throwaway Obsidian (see plugin-driver.mts)."""
    name, db = "plugin", "bench-plugin"

    def __init__(self, rep, resume=False):
        assert not resume, "resume is implemented for the official CLI only"
        self.root = os.path.join(WORK, "plugin", f"r{rep}")
        shutil.rmtree(self.root, ignore_errors=True)
        os.makedirs(self.root)
        env = dict(E, BENCH_DB=self.db, BENCH_ROOT=self.root,
                   OBSIDIAN_BINARY=E.get("OBSIDIAN_BINARY", f"{OBSIDIAN_DIR}/squashfs-root/obsidian"),
                   OBSIDIAN_CLI=E.get("OBSIDIAN_CLI", f"{OBSIDIAN_DIR}/squashfs-root/obsidian-cli"),
                   E2E_OBSIDIAN_REMOTE_DEBUGGING_PORT=E.get("PLUGIN_CDP_PORT", "9333"),
                   E2E_OBSIDIAN_VAULT_TIMEOUT_MS="60000", E2E_OBSIDIAN_UI_TIMEOUT_MS="60000")
        self.drv_log = open(os.path.join(self.root, "driver.log"), "ab")
        self.drv = subprocess.Popen([os.path.join(REF, "node_modules/.bin/tsx"), os.path.join(HERE, "plugin-driver.mts")],
                                    cwd=REF, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                    stderr=self.drv_log, text=True, start_new_session=True)
        self.pid = None
        self.vault = self.user_data = None

    def call(self, **msg):
        self.drv.stdin.write(json.dumps(msg) + "\n")
        self.drv.stdin.flush()
        line = self.drv.stdout.readline()
        if not line:
            raise RuntimeError("plug-in driver exited; see driver.log")
        r = json.loads(line)
        if not r.get("ok"):
            raise RuntimeError(f"plug-in driver: {r.get('error')}")
        return r

    def launch(self, live):
        r = self.call(cmd="start", liveSync=live)
        self.pid, self.vault, self.user_data = r["pid"], r["vault"], r["userData"]
        return r

    def cold(self):
        t0 = time.monotonic()
        r = self.launch(False)
        launch_s = time.monotonic() - t0
        c, s = Counters(), Sampler(self.pid)
        s.start()
        t1 = time.monotonic()
        f = self.call(cmd="fetch")
        wall = time.monotonic() - t1
        res = {"wall_s": round(wall, 2), "launch_s": round(launch_s, 2), **s.done(), **c.done(),
               "rc": 0, "fetch_ms": f["fetch_ms"], "launch_ms": r["launch_ms"]}
        self.stop()
        res["state_mb"] = self.state_mb()
        return res

    def start(self):
        self.launch(True)
        return self.pid

    def stop(self):
        if self.pid:
            self.call(cmd="stop")
        self.pid = None

    def local_write(self, rel, body):
        Lsync.local_write(self, rel, body)

    def state_mb(self):
        # PouchDB lives in the renderer's IndexedDB inside the isolated --user-data-dir.
        return round(du(os.path.join(self.user_data, "IndexedDB")) / 1e6, 1) if self.user_data else None

    def close(self):
        try:
            self.call(cmd="quit")
        except Exception as e:
            log("plug-in quit:", e)
        self.drv.wait(60)


CONTENDERS = {"lsync": Lsync, "official": Official, "plugin": Plugin}


# --- the sequence -------------------------------------------------------------------------
def run_rep(cls, rep, args):
    if args.resume:
        # Continue an earlier repetition's database and client state (no clone, no cold sync).
        c = cls(rep, resume=True)
    else:
        clone(cls.db)
        c = cls(rep)
    base = {"contender": c.name, "rep": rep, "db": c.db}
    try:
        if not args.resume:
            log(f"[{c.name} r{rep}] cold")
            cold = c.cold()
            n, size = vault_stats(c.vault)
            record({**base, "test": "cold", **cold, "vault_files": n, "vault_mb": round(size / 1e6, 1)})
            tree_file = os.path.join(WORK, "trees", f"{c.name}-r{rep}.json")
            os.makedirs(os.path.dirname(tree_file), exist_ok=True)
            with open(tree_file, "w") as f:
                json.dump(digest_tree(c.vault), f)
            if args.stages == ["cold"]:
                return
        w = Writer(c.db, os.path.join(c.root, "writer"))
        w.sync()
        for n_changes in (args.inc if "inc" in args.stages else []):
            tag = f"inc{n_changes}-r{rep}"
            want = {}
            for i in range(n_changes):
                rel = f"bench-writes/{tag}/f{i:04d}.md"
                w.put(rel, note_body(tag, i))
                want[os.path.join(c.vault, rel)] = note_body(tag, i)
            w.sync()
            cnt = Counters()
            t0 = time.monotonic()
            pid = c.start()
            s = Sampler(pid, every=0.5)
            s.start()
            t1 = wait_files(want, timeout=args.inc_timeout.get(c.name, args.timeout))
            rec = {**base, "test": f"inc-{n_changes}", "wall_s": round(t1 - t0, 2) if t1 else None,
                   **s.done(), **cnt.done()}
            record(rec)
            if n_changes != args.inc[-1]:
                c.stop()
        if "inc" not in args.stages and ("idle" in args.stages or "live" in args.stages):
            c.start()
        # The daemon is running now. Let it settle, then measure idle.
        if "idle" in args.stages:
            time.sleep(args.settle)
            cnt, s = Counters(), Sampler(c.pid, every=5)
            s.start()
            time.sleep(args.idle)
            record({**base, "test": "idle", "seconds": args.idle, **s.done(), **cnt.done(), "state_mb": c.state_mb()})
        if "live" not in args.stages:
            return
        # Live latency.
        fol = Follower(c.db)
        fol.start()
        s2l, l2s = [], []
        for i in range(args.live):
            rel = f"bench-writes/live-r{rep}/s2l-{i:02d}.md"
            body = note_body(f"s2l r{rep}", i)
            w.put(rel, body)
            box = {}
            th = threading.Thread(target=lambda: box.setdefault("t", wait_files({os.path.join(c.vault, rel): body}, 60, 0.002)))
            th.start()
            w.sync()
            th.join()
            tc = fol.wait_for(rel.lower(), 10)
            s2l.append(round(box["t"] - tc, 3) if box.get("t") and tc else None)
            time.sleep(1)
        for i in range(args.live):
            rel = f"bench-local/l2s-r{rep}-{i:02d}.md"
            t0 = time.monotonic()
            c.local_write(rel, note_body(f"l2s r{rep}", i))
            tc = fol.wait_for(rel.lower(), 60)
            l2s.append(round(tc - t0, 3) if tc else None)
            time.sleep(1)
        fol.close()
        record({**base, "test": "live", "server_to_local_s": s2l, "local_to_server_s": l2s})
    finally:
        c.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--contenders", default="lsync,official,plugin")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--first-rep", type=int, default=1)
    ap.add_argument("--stages", default="cold,inc,idle,live", help="cold is always run, unless --resume")
    ap.add_argument("--resume", action="store_true", help="official only: continue an earlier repetition")
    ap.add_argument("--inc-timeout", default="", help="per contender, e.g. official=1800")
    ap.add_argument("--inc", default="10,1000")
    ap.add_argument("--settle", type=int, default=60)
    ap.add_argument("--idle", type=int, default=300)
    ap.add_argument("--live", type=int, default=10)
    ap.add_argument("--timeout", type=int, default=3600)
    args = ap.parse_args()
    args.inc = [int(x) for x in args.inc.split(",") if x]
    args.stages = args.stages.split(",")
    args.inc_timeout = dict((k, int(v)) for k, v in (x.split("=") for x in args.inc_timeout.split(",") if x))
    os.makedirs(WORK, exist_ok=True)
    snap = couch(f"/{E['SNAP_DB']}")
    record({"test": "snapshot", "db": E["SNAP_DB"], "doc_count": snap["doc_count"], "update_seq": snap["update_seq"][:32]})
    # Interleave contenders within each repetition, so slow drifts on the host hit all of them.
    for rep in range(args.first_rep, args.first_rep + args.reps):
        for name in args.contenders.split(","):
            try:
                run_rep(CONTENDERS[name], rep, args)
            except Exception as e:
                log(f"[{name} r{rep}] FAILED: {e!r}")
                record({"contender": name, "rep": rep, "test": "error", "error": repr(e)})


if __name__ == "__main__":
    main()
