#!/usr/bin/env python3
"""Benchmark the `oak` CLI against `git` on the same machine.

Two families of workloads:

* local  -- client-side commands on real public corpora (import, status,
            diff, commit, log, branch).
* forge  -- ForgeMark's workload shapes (https://github.com/entireio/forgemark)
            replayed with the real CLIs against local servers: `oak serve`
            versus `git daemon`. Strategies: `branch` (N agents push their own
            branch in a loop), `clone` (shallow clone in a loop) and `session`
            (shallow clone, push K checkpoints, abandon).

ForgeMark itself drives go-git against smart-HTTP forges, so it cannot target
an Oak server; this harness keeps its commit shape (1-10 files x 2 KiB per
checkpoint), its warmup/measure windows and its stats (ops/s, p50/p95/p99), but
runs `oak`/`git` processes so client-side cost is included, as agents see it.

Usage:
  bench.py --oak PATH --corpus DIR [--corpus DIR...] --out results.json
           [--lanes local,forge] [--runs N] [--duration S] [--concurrency 1,4,8]
"""

import argparse
import json
import os
import random
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time

ENV = dict(os.environ)
ENV.update(
    OAK_NO_UPDATE_CHECK="1",
    OAK_AUTHOR="bench",
    OAK_PROGRESS="never",
    NO_COLOR="1",
    GIT_AUTHOR_NAME="bench",
    GIT_AUTHOR_EMAIL="bench@example.com",
    GIT_COMMITTER_NAME="bench",
    GIT_COMMITTER_EMAIL="bench@example.com",
    GIT_CONFIG_NOSYSTEM="1",
    GIT_TERMINAL_PROMPT="0",
)


def sh(cmd, cwd=None, check=True, env=None):
    t = time.perf_counter()
    p = subprocess.run(
        cmd,
        cwd=cwd,
        env=env or ENV,
        shell=isinstance(cmd, str),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    dt = time.perf_counter() - t
    if check and p.returncode != 0:
        raise RuntimeError(
            "command failed (%d): %s\n%s\n%s"
            % (p.returncode, cmd, p.stdout.decode()[-2000:], p.stderr.decode()[-2000:])
        )
    return dt, p


def timed(cmd, cwd, runs, setup=None):
    samples = []
    for _ in range(runs):
        if setup:
            setup()
        samples.append(sh(cmd, cwd)[0] * 1000)
    return samples


def summarize(samples):
    if not samples:
        return {"n": 0}
    s = sorted(samples)

    def pct(p):
        return s[min(len(s) - 1, int(round(p / 100.0 * (len(s) - 1))))]

    return {
        "n": len(s),
        "median_ms": round(statistics.median(s), 2),
        "min_ms": round(s[0], 2),
        "p95_ms": round(pct(95), 2),
    }


def copy_corpus(src, dst):
    shutil.copytree(src, dst, ignore=shutil.ignore_patterns(".git"), symlinks=True)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def write_checkpoint(work, rng, tag):
    """ForgeMark's commit shape: 1-10 files of 2 KiB each."""
    os.makedirs(os.path.join(work, "fm"), exist_ok=True)
    for i in range(rng.randint(1, 10)):
        with open(os.path.join(work, "fm", "%s-%d.txt" % (tag, i)), "wb") as f:
            f.write(os.urandom(1024).hex().encode())


# --------------------------------------------------------------------------
# local lane


class Tool:
    name = ""

    def __init__(self, oak):
        self.oak = oak


class Git(Tool):
    name = "git"

    def init_import(self, d):
        return "git init -q -b main && git add -A && git commit -qm init"

    def status(self):
        return "git status --porcelain=v1 --untracked-files=all"

    def diff(self):
        return "git diff"

    def commit(self):
        return "git add -A && git commit -qm wip"

    def log(self):
        return "git log -n 10 --oneline"

    def branch(self, name):
        return "git switch -q -c %s" % name


class Oak(Tool):
    name = "oak"

    def init_import(self, d):
        return "%s init >/dev/null && %s commit >/dev/null" % (self.oak, self.oak)

    def status(self):
        return "%s status --porcelain" % self.oak

    def diff(self):
        return "%s diff --print" % self.oak

    def commit(self):
        return "%s commit --json --quiet" % self.oak

    def log(self):
        return "%s log -n 10 --oneline" % self.oak

    def branch(self, name):
        return "%s switch -c %s" % (self.oak, name)


def local_lane(tools, corpus, runs, tmp):
    rng = random.Random(7)
    name = os.path.basename(corpus.rstrip("/"))
    out = {}
    for tool in tools:
        res = {}
        work = os.path.join(tmp, "%s-%s" % (name, tool.name))
        # Import: fresh copy each run (only once for big corpora).
        import_runs = 1 if runs < 3 else 2
        samples = []
        for i in range(import_runs):
            shutil.rmtree(work, ignore_errors=True)
            copy_corpus(corpus, work)
            # Warm the page cache for the copy, like a just-written tree.
            sh("find . -type f -print0 | xargs -0 cat >/dev/null", work)
            samples.append(sh(tool.init_import(work), work)[0] * 1000)
        res["init_import"] = summarize(samples)

        sh(tool.status(), work)  # prime caches (index / stat cache)
        res["status_clean"] = summarize(timed(tool.status(), work, runs))

        files = sorted(
            os.path.join(r, f)
            for r, _, fs in os.walk(work)
            if "/.git" not in r and "/.oak" not in r
            for f in fs
        )
        target = files[len(files) // 2]

        def touch_one():
            with open(target, "ab") as f:
                f.write(b"\n// bench edit\n")

        touch_one()
        res["status_one_modified"] = summarize(timed(tool.status(), work, runs))
        res["diff_one_modified"] = summarize(timed(tool.diff(), work, runs))

        os.makedirs(os.path.join(work, "untracked"), exist_ok=True)
        for i in range(100):
            with open(os.path.join(work, "untracked", "u%d.txt" % i), "w") as f:
                f.write("u%d\n" % i)
        res["status_100_untracked"] = summarize(timed(tool.status(), work, runs))
        shutil.rmtree(os.path.join(work, "untracked"))

        counter = [0]

        def checkpoint():
            counter[0] += 1
            write_checkpoint(work, rng, "c%d" % counter[0])

        res["commit_checkpoint"] = summarize(
            timed(tool.commit(), work, runs, setup=checkpoint)
        )
        res["log_10"] = summarize(timed(tool.log(), work, runs))

        bcount = [0]
        samples = []
        for _ in range(runs):
            bcount[0] += 1
            samples.append(sh(tool.branch("b%d" % bcount[0]), work)[0] * 1000)
        res["branch_create"] = summarize(samples)

        res["repo_bytes"] = int(
            sh("du -sk %s" % (".git" if tool.name == "git" else ".oak"), work)[1]
            .stdout.split()[0]
        ) * 1024
        out[tool.name] = res
        shutil.rmtree(work, ignore_errors=True)
    return out


# --------------------------------------------------------------------------
# forge lane


class OakServer:
    name = "oak"

    def __init__(self, oak, tmp):
        self.oak = oak
        self.port = free_port()
        self.data = os.path.join(tmp, "oak-data")
        self.proc = subprocess.Popen(
            [oak, "serve", "-d", self.data, "-p", str(self.port)],
            env=ENV,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.url = "http://127.0.0.1:%d" % self.port
        wait_port(self.port)

    def seed(self, corpus, tmp):
        work = os.path.join(tmp, "seed-oak")
        copy_corpus(corpus, work)
        o = self.oak
        sh("%s init >/dev/null && %s commit >/dev/null" % (o, o), work)
        dt, _ = sh("%s push -r %s --repo bench/repo" % (o, self.url), work)
        sh("%s merge" % o, work)
        shutil.rmtree(work, ignore_errors=True)
        return dt * 1000

    def clone(self, dest, shallow=True):
        return "%s clone %s/bench/repo %s %s" % (
            self.oak,
            self.url,
            dest,
            "--shallow" if shallow else "",
        )

    def new_branch(self, name):
        return "%s switch -c %s" % (self.oak, name)

    def commit_push(self, name):
        return "%s commit --json --quiet >/dev/null && %s push --json >/dev/null" % (
            self.oak,
            self.oak,
        )

    def stop(self):
        self.proc.terminate()
        self.proc.wait()


class GitServer:
    name = "git"

    def __init__(self, tmp):
        self.base = os.path.join(tmp, "git-data")
        os.makedirs(self.base)
        self.port = free_port()
        self.proc = subprocess.Popen(
            [
                "git",
                "daemon",
                "--reuseaddr",
                "--export-all",
                "--enable=receive-pack",
                "--base-path=%s" % self.base,
                "--listen=127.0.0.1",
                "--port=%d" % self.port,
                self.base,
            ],
            env=ENV,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.url = "git://127.0.0.1:%d" % self.port
        wait_port(self.port)

    def seed(self, corpus, tmp):
        sh("git init -q --bare -b main repo.git", self.base)
        work = os.path.join(tmp, "seed-git")
        copy_corpus(corpus, work)
        sh("git init -q -b main && git add -A && git commit -qm init", work)
        dt, _ = sh("git push -q %s/repo.git main" % self.url, work)
        shutil.rmtree(work, ignore_errors=True)
        return dt * 1000

    def clone(self, dest, shallow=True):
        return "git clone -q %s %s/repo.git %s" % (
            "--depth 1" if shallow else "",
            self.url,
            dest,
        )

    def new_branch(self, name):
        return "git switch -q -c %s" % name

    def commit_push(self, name):
        return "git add -A && git commit -qm wip && git push -q -f origin %s" % name

    def stop(self):
        self.proc.terminate()
        self.proc.wait()


def wait_port(port, timeout=10):
    end = time.time() + timeout
    while time.time() < end:
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("server on port %d never came up" % port)


def run_agents(conc, warmup, duration, body):
    """Run `body(agent_id, stop_event)` loops; each yields (kind, ms, ok)."""
    samples = []
    lock = threading.Lock()
    start = time.time()
    measure_from = start + warmup
    stop_at = measure_from + duration
    errors = []

    def worker(i):
        rng = random.Random(i)
        state = {}
        while time.time() < stop_at:
            try:
                for kind, ms in body(i, rng, state):
                    now = time.time()
                    if now >= measure_from and now <= stop_at:
                        with lock:
                            samples.append((kind, ms))
            except Exception as e:  # noqa: BLE001
                with lock:
                    errors.append(str(e)[:400])
                time.sleep(0.05)

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(conc)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    by_kind = {}
    for kind, ms in samples:
        by_kind.setdefault(kind, []).append(ms)
    out = {}
    for kind, ms in by_kind.items():
        s = sorted(ms)
        out[kind] = {
            "ops": len(s),
            "ops_per_s": round(len(s) / duration, 2),
            "p50_ms": round(s[len(s) // 2], 1),
            "p95_ms": round(s[int(0.95 * (len(s) - 1))], 1),
            "p99_ms": round(s[int(0.99 * (len(s) - 1))], 1),
        }
    out["errors"] = len(errors)
    if errors:
        out["first_error"] = errors[0]
    return out


def forge_lane(server, corpus, tmp, concurrencies, warmup, duration, session_commits):
    res = {"seed_push_ms": round(server.seed(corpus, tmp), 1)}
    agents_dir = os.path.join(tmp, "agents-" + server.name)
    os.makedirs(agents_dir)
    seq = [0]
    seq_lock = threading.Lock()

    def next_id():
        with seq_lock:
            seq[0] += 1
            return seq[0]

    # full clone once (cold, single)
    d = os.path.join(agents_dir, "full")
    res["full_clone_ms"] = round(sh(server.clone(d, shallow=False), agents_dir)[0] * 1000, 1)
    shutil.rmtree(d)

    def branch_body(i, rng, state):
        if "work" not in state:
            w = os.path.join(agents_dir, "branch-%d-%d" % (i, next_id()))
            sh(server.clone(w), agents_dir)
            name = "fm-%d-%d" % (i, next_id())
            sh(server.new_branch(name), w)
            state.update(work=w, name=name, n=0)
        state["n"] += 1
        write_checkpoint(state["work"], rng, "a%d-%d" % (i, state["n"]))
        dt, _ = sh(server.commit_push(state["name"]), state["work"])
        yield "push", dt * 1000

    def clone_body(i, rng, state):
        w = os.path.join(agents_dir, "clone-%d-%d" % (i, next_id()))
        dt, _ = sh(server.clone(w), agents_dir)
        shutil.rmtree(w, ignore_errors=True)
        yield "clone", dt * 1000

    def session_body(i, rng, state):
        w = os.path.join(agents_dir, "session-%d-%d" % (i, next_id()))
        dt, _ = sh(server.clone(w), agents_dir)
        yield "clone", dt * 1000
        name = "s-%d-%d" % (i, next_id())
        sh(server.new_branch(name), w)
        for k in range(session_commits):
            write_checkpoint(w, rng, "s%d" % k)
            dt, _ = sh(server.commit_push(name), w)
            yield "push", dt * 1000
        shutil.rmtree(w, ignore_errors=True)

    for strategy, body in (
        ("branch", branch_body),
        ("clone", clone_body),
        ("session", session_body),
    ):
        res[strategy] = {}
        for c in concurrencies:
            res[strategy][str(c)] = run_agents(c, warmup, duration, body)
            print("  %s %s c=%d %s" % (server.name, strategy, c, res[strategy][str(c)]),
                  flush=True)
    shutil.rmtree(agents_dir, ignore_errors=True)
    return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oak", required=True)
    ap.add_argument("--corpus", action="append", required=True)
    ap.add_argument("--forge-corpus", help="corpus seeded into the servers (default: first)")
    ap.add_argument("--out", required=True)
    ap.add_argument("--lanes", default="local,forge")
    ap.add_argument("--runs", type=int, default=10)
    ap.add_argument("--duration", type=float, default=20)
    ap.add_argument("--warmup", type=float, default=3)
    ap.add_argument("--concurrency", default="1,4,8")
    ap.add_argument("--session-commits", type=int, default=5)
    ap.add_argument("--tools", default="git,oak")
    a = ap.parse_args()

    lanes = a.lanes.split(",")
    tools = [t for t in (Git(a.oak), Oak(a.oak)) if t.name in a.tools.split(",")]
    report = {
        "meta": {
            "oak_version": sh([a.oak, "--version"])[1].stdout.decode().strip(),
            "git_version": sh(["git", "--version"])[1].stdout.decode().strip(),
            "host": sh("sysctl -n machdep.cpu.brand_string", check=False)[1].stdout.decode().strip(),
            "ncpu": os.cpu_count(),
            "args": vars(a),
            "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        },
        "local": {},
        "forge": {},
    }
    tmp = tempfile.mkdtemp(prefix="oak-vs-git-")
    try:
        if "local" in lanes:
            for corpus in a.corpus:
                name = os.path.basename(corpus.rstrip("/"))
                print("local:", name, flush=True)
                report["local"][name] = local_lane(tools, corpus, a.runs, tmp)
                print(json.dumps(report["local"][name], indent=1), flush=True)
        if "forge" in lanes:
            corpus = a.forge_corpus or a.corpus[0]
            conc = [int(c) for c in a.concurrency.split(",")]
            report["forge"]["corpus"] = os.path.basename(corpus.rstrip("/"))
            for tname in a.tools.split(","):
                ftmp = os.path.join(tmp, "forge-" + tname)
                os.makedirs(ftmp)
                server = OakServer(a.oak, ftmp) if tname == "oak" else GitServer(ftmp)
                try:
                    print("forge:", tname, flush=True)
                    report["forge"][tname] = forge_lane(
                        server, corpus, ftmp, conc, a.warmup, a.duration, a.session_commits
                    )
                finally:
                    server.stop()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    with open(a.out, "w") as f:
        json.dump(report, f, indent=2)
    print("wrote", a.out)


if __name__ == "__main__":
    sys.exit(main())
