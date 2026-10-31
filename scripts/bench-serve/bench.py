#!/usr/bin/env python3
"""Benchmark a self-hosted `oak serve` with synthetic repositories.

Unlike `scripts/bench-vs-git` (real public corpora, oak vs git), this harness
needs nothing but an `oak` binary: it generates deterministic repos of N small
files and measures the server paths a self-hoster pays for, with real CLI
processes so client cost is included, as agents see it:

  seed_push      first publication of an N-file repo (init + commit + push)
  merge          squash-merge of the seeded branch onto main
  clone          full clone of main
  clone_shallow  `clone --shallow`
  push_1         median of 1-file checkpoint pushes on a fresh branch
  pull_1         median pull that brings in one new main commit
  push_cN        push throughput with N agents, each on its own branch
  large_*        push + clone of one large random file (chunked path)

Usage:
  bench.py --oak PATH [--sizes 1000,10000] [--concurrency 1,4,8]
           [--duration 10] [--runs 5] [--large-mb 64] [--out results.json]

On macOS copy a rebuilt `oak` to a new path first: overwriting a signed binary
in place gets the next exec SIGKILLed.
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
)
ENV.pop("OAK_API_KEY", None)


def sh(cmd, cwd=None):
    t = time.perf_counter()
    p = subprocess.run(cmd, cwd=cwd, env=ENV, shell=True, capture_output=True)
    dt = (time.perf_counter() - t) * 1000
    if p.returncode != 0:
        raise RuntimeError(
            "command failed (%d): %s\n%s\n%s"
            % (p.returncode, cmd, p.stdout.decode()[-2000:], p.stderr.decode()[-2000:])
        )
    return dt


def stats(samples):
    s = sorted(samples)
    if not s:
        return {"n": 0}
    return {
        "n": len(s),
        "p50_ms": round(statistics.median(s), 1),
        "min_ms": round(s[0], 1),
        "p95_ms": round(s[int(0.95 * (len(s) - 1))], 1),
    }


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def gen_corpus(path, files, seed=1):
    """N ~2 KiB text files, 100 per directory, deterministic content."""
    rng = random.Random(seed)
    alphabet = bytes(b"abcdefghij \n" * 22)[:256]
    for i in range(files):
        d = os.path.join(path, "src", "d%04d" % (i // 100))
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, "f%06d.txt" % i), "wb") as f:
            f.write(b"%d\n" % i)
            f.write(rng.randbytes(2048).translate(alphabet))


class Server:
    def __init__(self, oak, tmp):
        self.oak = oak
        self.port = free_port()
        self.proc = subprocess.Popen(
            [oak, "serve", "-d", os.path.join(tmp, "data"), "-p", str(self.port)],
            env=ENV,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.url = "http://127.0.0.1:%d" % self.port
        end = time.time() + 15
        while time.time() < end:
            try:
                socket.create_connection(("127.0.0.1", self.port), 0.2).close()
                return
            except OSError:
                time.sleep(0.05)
        raise RuntimeError("oak serve never came up")

    def stop(self):
        self.proc.terminate()
        self.proc.wait()


def bench_size(oak, server, files, tmp, runs, concurrency, duration):
    repo = "bench/r%d" % files
    res = {"files": files}
    seed = os.path.join(tmp, "seed-%d" % files)
    gen_corpus(seed, files)
    sh("%s init >/dev/null && %s commit >/dev/null" % (oak, oak), seed)
    res["seed_push_ms"] = round(sh("%s push -r %s --repo %s" % (oak, server.url, repo), seed), 1)
    res["merge_ms"] = round(sh("%s merge" % oak, seed), 1)

    clones = os.path.join(tmp, "clones-%d" % files)
    os.makedirs(clones)
    samples = []
    for i in range(runs):
        dest = os.path.join(clones, "full-%d" % i)
        samples.append(sh("%s clone %s/%s %s" % (oak, server.url, repo, dest), clones))
        shutil.rmtree(dest)
    res["clone"] = stats(samples)
    samples = []
    for i in range(runs):
        dest = os.path.join(clones, "shallow-%d" % i)
        samples.append(sh("%s clone --shallow %s/%s %s" % (oak, server.url, repo, dest), clones))
        shutil.rmtree(dest)
    res["clone_shallow"] = stats(samples)

    work = os.path.join(clones, "work")
    sh("%s clone %s/%s %s" % (oak, server.url, repo, work), clones)
    sh("%s switch -c push1 >/dev/null" % oak, work)
    samples = []
    for i in range(runs):
        with open(os.path.join(work, "src", "edit-%d.txt" % i), "w") as f:
            f.write("edit %d\n" % i)
        samples.append(sh("%s commit --json --quiet >/dev/null && %s push --json >/dev/null" % (oak, oak), work))
    res["push_1"] = stats(samples)

    # pull_1: another clone lands one commit on main; `work`'s main pulls it.
    lander = os.path.join(clones, "lander")
    sh("%s clone %s/%s %s" % (oak, server.url, repo, lander), clones)
    puller = os.path.join(clones, "puller")
    sh("%s clone %s/%s %s" % (oak, server.url, repo, puller), clones)
    samples = []
    for i in range(runs):
        sh("%s switch -c land-%d >/dev/null" % (oak, i), lander)
        with open(os.path.join(lander, "landed-%d.txt" % i), "w") as f:
            f.write("landed %d\n" % i)
        sh("%s commit --quiet >/dev/null && %s push >/dev/null && %s merge >/dev/null" % (oak, oak, oak), lander)
        samples.append(sh("%s pull --json >/dev/null" % oak, puller))
    res["pull_1"] = stats(samples)

    for c in concurrency:
        res["push_c%d" % c] = push_throughput(oak, server, repo, clones, c, duration)
    shutil.rmtree(seed, ignore_errors=True)
    shutil.rmtree(clones, ignore_errors=True)
    return res


def push_throughput(oak, server, repo, clones, conc, duration):
    """`conc` agents each push 1-file checkpoints to their own branch."""
    agents = []
    for i in range(conc):
        w = os.path.join(clones, "c%d-agent%d" % (conc, i))
        sh("%s clone --shallow %s/%s %s" % (oak, server.url, repo, w), clones)
        sh("%s switch -c c%d-agent%d >/dev/null" % (oak, conc, i), w)
        agents.append(w)
    samples, errors = [], []
    lock = threading.Lock()
    stop_at = time.time() + duration

    def worker(w):
        n = 0
        while time.time() < stop_at:
            n += 1
            with open(os.path.join(w, "cp-%d.txt" % n), "w") as f:
                f.write(os.urandom(1024).hex())
            try:
                ms = sh("%s commit --json --quiet >/dev/null && %s push --json >/dev/null" % (oak, oak), w)
                with lock:
                    samples.append(ms)
            except RuntimeError as e:
                with lock:
                    errors.append(str(e)[:300])

    threads = [threading.Thread(target=worker, args=(w,)) for w in agents]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    out = stats(samples)
    out["ops_per_s"] = round(len(samples) / duration, 2)
    out["errors"] = len(errors)
    if errors:
        out["first_error"] = errors[0]
    return out


def bench_large(oak, server, mb, tmp):
    repo = "bench/large%d" % mb
    work = os.path.join(tmp, "large")
    os.makedirs(work)
    with open(os.path.join(work, "asset.bin"), "wb") as f:
        rng = random.Random(7)
        for _ in range(mb):
            f.write(rng.randbytes(1 << 20))
    sh("%s init >/dev/null && %s commit >/dev/null" % (oak, oak), work)
    res = {"mb": mb}
    res["large_push_ms"] = round(sh("%s push -r %s --repo %s" % (oak, server.url, repo), work), 1)
    sh("%s merge" % oak, work)
    dest = os.path.join(tmp, "large-clone")
    res["large_clone_ms"] = round(sh("%s clone %s/%s %s" % (oak, server.url, repo, dest), tmp), 1)
    shutil.rmtree(work, ignore_errors=True)
    shutil.rmtree(dest, ignore_errors=True)
    return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oak", required=True)
    ap.add_argument("--sizes", default="1000,10000")
    ap.add_argument("--concurrency", default="1,4,8")
    ap.add_argument("--duration", type=float, default=10)
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--large-mb", type=int, default=64)
    ap.add_argument("--out")
    a = ap.parse_args()
    oak = os.path.abspath(a.oak)
    report = {
        "meta": {
            "oak_version": subprocess.run([oak, "--version"], capture_output=True).stdout.decode().strip(),
            "ncpu": os.cpu_count(),
            "args": vars(a),
            "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        },
        "sizes": [],
    }
    tmp = tempfile.mkdtemp(prefix="oak-bench-serve-")
    server = Server(oak, tmp)
    try:
        conc = [int(c) for c in a.concurrency.split(",") if c]
        for files in [int(s) for s in a.sizes.split(",") if s]:
            r = bench_size(oak, server, files, tmp, a.runs, conc, a.duration)
            print(json.dumps(r), flush=True)
            report["sizes"].append(r)
        if a.large_mb:
            report["large"] = bench_large(oak, server, a.large_mb, tmp)
            print(json.dumps(report["large"]), flush=True)
    finally:
        server.stop()
        shutil.rmtree(tmp, ignore_errors=True)
    if a.out:
        with open(a.out, "w") as f:
            json.dump(report, f, indent=2)
        print("wrote", a.out)


if __name__ == "__main__":
    sys.exit(main())
