#!/usr/bin/env python3
"""Many Mercurial or Subversion repositories of mixed sizes and skewed activity against one slates daemon, driven by
the tools' stock clients (A-724 piece 7, A-849; §4.17.5 "Proof"; main 01a20182's ruling of 2026-11-21 21:35: option (a),
a smaller-scale stock-client driver, its counts derived, the stock clients the proof). `docs/wip/BENCHMARKS.md` records
each run; the command is the contract. git's raw-protocol driver is `multi_repo_load.py`.

The repositories. A step of R repositories gives the repository of rank i the size class `multi_repo_load.class_of`
gives it (rank 1 the largest, a power law by rank). A class's content is the tree of one revision of a git SOURCE
(`git archive`), smallest first, so the three tools are measured over the same bytes.

What a step does. Each repository is made and seeded by its own working copy: the class's tree committed and pushed
once (unmeasured). Then, all at once: each repository's pushes, its Zipf share of R (`multi_repo_load.pushes_by_zipf`),
each one small changeset (a file of its own) committed and pushed from that repository's working copy, one after
another (a stock client's working copy takes one push at a time); and fetches by the same Zipf law, each a fresh stock
clone (`hg clone`) or checkout (`svn checkout`) of the repository. Per class: the pushes' and the fetches' wall times at
p50 and p99, each against the class's time alone, and the refusals in the client's own words.

The bounds, derived, no number picked (main's ruling). Each class's alone run measures a clone's bytes on disk and a
stock client's peak resident set. A step is not run when its working copies and clones would pass the scratch
filesystem's free bytes; its fetches run at most as many at once as free memory holds clients of the largest peak
measured; a step stops the run when the daemon is gone, when more than half its works are refused, or when macOS
reports memory pressure above normal (`multi_repo_load.pressed`). Each step runs on a fresh daemon and its scratch is
removed when it ends.

Scratch: ~/.cache/multi-repo-stock/<pid> (never /tmp), removed at the end, one literal path.

Usage: multi_repo_stock.py SLATES hg|svn SOURCE REV [REV ...] [--steps 1,10,100]
  SLATES  the release `slates` binary
  hg|svn  the tool; hg is the command SLATES_TEST_HG names (default `hg`), stock 7.2; svn is svn 1.15 on the PATH
  SOURCE  a git repository holding every REV (a bare clone of the dataset)
  REV     one revision a size class, smallest first (e.g. v0.99 v1.0.0 v1.5.0 of git/git)
  --steps the repository counts of the steps, in order (default 1,10,100: each stopped by the bounds above)
"""

import asyncio
import os
import platform
import resource
import shutil
import subprocess
import sys
import time

from multi_repo_load import class_of, free_mib, pressed, pushes_by_zipf, quantile
from push_load import Daemon, rss_kib

SCRATCH = os.path.join(os.path.expanduser("~/.cache/multi-repo-stock"), str(os.getpid()))


def tree_bytes(path: str) -> int:
    """The bytes of every file under PATH."""
    total = 0
    for root, _, files in os.walk(path):
        for name in files:
            try:
                total += os.lstat(os.path.join(root, name)).st_size
            except OSError:
                pass
    return total


def children_peak_kib() -> int:
    """The largest resident set any child of this process has reached, in KiB (macOS reports bytes, Linux KiB)."""
    peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    return peak // 1024 if sys.platform == "darwin" else peak


async def run(words: list[str], cwd: str, env: dict) -> tuple[float, str]:
    """A stock client's command: its wall time, and `ok` or its last error line."""
    start = time.perf_counter()
    process = await asyncio.create_subprocess_exec(
        *words, cwd=cwd, env=env, stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
    )
    out, err = await process.communicate()
    took = time.perf_counter() - start
    if process.returncode == 0:
        return took, "ok"
    lines = [line for line in (err + out).decode("utf-8", "replace").splitlines() if line.strip()]
    return took, (lines[-1] if lines else f"exit {process.returncode}")[:120]


class Tool:
    """One tool's stock client over one daemon: its repository verbs, a seed, a push of one small changeset, a fetch."""

    def __init__(self, name: str, daemon: Daemon):
        self.name, self.daemon = name, daemon
        self.env = dict(os.environ)
        if name == "hg":
            self.client = os.environ.get("SLATES_TEST_HG", "hg").split()
            self.env.update({"HGPLAIN": "1", "HGRCPATH": "", "HGUSER": "load <load@example.com>"})
            prefix = f"http://127.0.0.1:{daemon.port}/"
            self.options = []
            for item in (f"auth.s.prefix={prefix}", "auth.s.username=slates", f"auth.s.password={daemon.token}"):
                self.options += ["--config", item]
        else:
            self.client = ["svn"]
            self.options = ["--non-interactive", "--no-auth-cache", "--config-dir", os.path.join(SCRATCH, "svn-config"),
                            "--username", daemon.token_id, "--password", daemon.token]

    def url(self, repository: str) -> str:
        if self.name == "hg":
            return f"http://127.0.0.1:{self.daemon.port}/{repository}"
        return f"svn://127.0.0.1:{self.daemon.svn_port}/{repository}"

    def words(self, *args: str) -> list[str]:
        return [*self.client, *args, *self.options]

    def create(self, repository: str) -> None:
        self.daemon.cli("repository", "create", repository, "--tool", self.name)

    async def seed(self, repository: str, tree: str, copy: str) -> tuple[float, str]:
        """A working copy of REPOSITORY at COPY holding TREE, committed and pushed once."""
        verb = "clone" if self.name == "hg" else "checkout"
        took, words = await run(self.words(verb, self.url(repository), copy), SCRATCH, self.env)
        if words != "ok":
            return took, words
        shutil.copytree(tree, copy, dirs_exist_ok=True)
        if self.name == "hg":
            steps = [self.words("commit", "--addremove", "--message", "seed"), self.words("push", self.url(repository))]
        else:
            steps = [self.words("add", "--force", "--quiet", "."), self.words("commit", "--message", "seed")]
        for step in steps:
            more, words = await run(step, copy, self.env)
            took += more
            if words != "ok":
                return took, words
        return took, "ok"

    async def push(self, repository: str, copy: str, k: int) -> tuple[float, str]:
        """One small changeset, a file of its own, committed and pushed from COPY."""
        name = f"load-{k}.txt"
        with open(os.path.join(copy, name), "w") as file:
            file.write(f"{repository} {k}\n")
        if self.name == "hg":
            steps = [self.words("commit", "--addremove", "--message", f"load {k}"), self.words("push", self.url(repository))]
        else:
            steps = [self.words("add", "--quiet", name), self.words("commit", "--message", f"load {k}")]
        took = 0.0
        for step in steps:
            more, words = await run(step, copy, self.env)
            took += more
            if words != "ok":
                return took, words
        return took, "ok"

    async def fetch(self, repository: str, into: str) -> tuple[float, str]:
        """A fresh clone or checkout of REPOSITORY into INTO."""
        verb = "clone" if self.name == "hg" else "checkout"
        return await run(self.words(verb, self.url(repository), into), SCRATCH, self.env)


async def pushes_of(tool: Tool, repository: str, copy: str, count: int) -> list[tuple[float, str]]:
    """COUNT pushes from one working copy, one after another."""
    return [await tool.push(repository, copy, k) for k in range(count)]


async def fetch_bounded(tool: Tool, gate: asyncio.Semaphore, repository: str, into: str) -> tuple[float, str]:
    async with gate:
        return await tool.fetch(repository, into)


def fresh(slates: str, tool_name: str, label: str) -> tuple[Daemon, Tool]:
    daemon = Daemon(slates, instance=f"multi-stock-{os.getpid()}-{label}", svn=tool_name == "svn")
    return daemon, Tool(tool_name, daemon)


def main():
    args = sys.argv[1:]
    steps = [1, 10, 100]
    if "--steps" in args:
        at = args.index("--steps")
        steps = [int(n) for n in args[at + 1].split(",")]
        del args[at : at + 2]
    if len(args) < 4 or args[1] not in ("hg", "svn"):
        raise SystemExit(__doc__)
    slates, tool_name, source, *revs = args
    os.makedirs(os.path.join(SCRATCH, "svn-config"), exist_ok=True)
    try:
        run_all(slates, tool_name, source, revs, steps)
    finally:
        shutil.rmtree(SCRATCH, ignore_errors=True)


def run_all(slates: str, tool_name: str, source: str, revs: list[str], steps: list[int]) -> None:
    trees = []
    for size, rev in enumerate(revs):
        tree = os.path.join(SCRATCH, "classes", str(size))
        os.makedirs(tree)
        archive = subprocess.run(["git", "-C", source, "archive", rev], check=True, stdout=subprocess.PIPE).stdout
        subprocess.run(["tar", "-x", "-C", tree], input=archive, check=True)
        trees.append(tree)
    print(f"machine: {platform.platform()} {platform.processor()}, {os.cpu_count()} cores, load {os.getloadavg()}, "
          f"free {free_mib():,.0f} MiB, scratch free {shutil.disk_usage(SCRATCH).free / 2**20:,.0f} MiB; tool {tool_name}")
    for size, tree in enumerate(trees):
        print(f"class {size}: {revs[size]}, {tree_bytes(tree):,} bytes in its tree", flush=True)

    # Alone: each class on its own daemon: the seed, one push and one fetch, the baselines; a clone's bytes on disk and
    # a client's peak resident set, the bounds' inputs.
    push_alone, fetch_alone, clone_bytes = [], [], []
    for size, tree in enumerate(trees):
        daemon, tool = fresh(slates, tool_name, f"alone{size}")
        try:
            tool.create("alone")
            copy = os.path.join(SCRATCH, f"alone{size}-copy")
            seeded = asyncio.run(tool.seed("alone", tree, copy))
            pushed = asyncio.run(tool.push("alone", copy, 0))
            into = os.path.join(SCRATCH, f"alone{size}-fetch")
            fetched = asyncio.run(tool.fetch("alone", into))
            clone_bytes.append(tree_bytes(into))
            shutil.rmtree(copy, ignore_errors=True)
            shutil.rmtree(into, ignore_errors=True)
        finally:
            daemon.stop()
        push_alone.append(pushed[0])
        fetch_alone.append(fetched[0])
        print(f"alone class {size}: seed {seeded[0]:.3f} s {seeded[1]}, push {pushed[0]:.3f} s {pushed[1]}, "
              f"fetch {fetched[0]:.3f} s {fetched[1]}, a clone {clone_bytes[-1] / 2**20:,.1f} MiB", flush=True)
    client_kib = max(1, children_peak_kib())
    print(f"a stock client's peak resident set: {client_kib / 1024:,.1f} MiB", flush=True)

    for repositories in steps:
        counts = pushes_by_zipf(repositories)
        classes = [class_of(rank, len(trees)) for rank in range(1, repositories + 1)]
        disk = sum(clone_bytes[size] * (1 + count) for size, count in zip(classes, counts))
        free_disk = shutil.disk_usage(SCRATCH).free
        if disk > free_disk:
            print(f"R={repositories}: its copies need {disk / 2**20:,.0f} MiB past {free_disk / 2**20:,.0f} MiB free; stopped")
            break
        if pressed():
            print(f"R={repositories}: memory pressure above normal before the step; stopped")
            break
        at_once = max(1, int(free_mib() * 1024 // client_kib))
        step_dir = os.path.join(SCRATCH, f"r{repositories}")
        os.makedirs(step_dir)
        daemon, tool = fresh(slates, tool_name, f"r{repositories}")
        try:
            names = [f"r{rank}" for rank in range(1, repositories + 1)]
            for name in names:
                tool.create(name)
            copies = [os.path.join(step_dir, f"{name}-copy") for name in names]

            async def seed_all():
                gate = asyncio.Semaphore(at_once)

                async def one(name, tree, copy):
                    async with gate:
                        return await tool.seed(name, tree, copy)

                return await asyncio.gather(*(one(n, trees[c], p) for n, c, p in zip(names, classes, copies)))

            seeded = asyncio.run(seed_all())
            unseeded = sum(1 for _, words in seeded if words != "ok")
            if unseeded:
                print(f"R={repositories}: {unseeded} of {len(seeded)} seeds refused: "
                      f"{next(words for _, words in seeded if words != 'ok')}", flush=True)

            async def burst():
                gate = asyncio.Semaphore(at_once)
                pushing = [pushes_of(tool, n, p, c) for n, p, c in zip(names, copies, counts)]
                fetching = [
                    fetch_bounded(tool, gate, n, os.path.join(step_dir, f"{n}-fetch{k}"))
                    for n, c in zip(names, counts) for k in range(c)
                ]
                start = time.perf_counter()
                pushed, fetched = await asyncio.gather(asyncio.gather(*pushing), asyncio.gather(*fetching))
                return pushed, fetched, time.perf_counter() - start

            before = rss_kib(daemon.pid)
            pushed, fetched, wall = asyncio.run(burst())
            peak = rss_kib(daemon.pid)
            alive = peak > 0
        finally:
            daemon.stop()
            shutil.rmtree(step_dir, ignore_errors=True)
        push_results = [(size, took, words) for size, works in zip(classes, pushed) for took, words in works]
        fetch_classes = [size for size, count in zip(classes, counts) for _ in range(count)]
        fetch_results = [(size, took, words) for size, (took, words) in zip(fetch_classes, fetched)]
        total = len(push_results) + len(fetch_results)
        refused = sum(1 for _, _, words in push_results + fetch_results if words != "ok")
        print(f"R={repositories}: {len(push_results)} pushes, {len(fetch_results)} fetches ({at_once} at once at most), "
              f"wall {wall:.3f} s, daemon rss {before / 1024:.0f} -> {peak / 1024:.0f} MiB, refused {refused}, "
              f"daemon {'alive' if alive else 'GONE'}", flush=True)
        for size in range(len(trees)):
            for kind, results, alone in (("pushes", push_results, push_alone), ("fetches", fetch_results, fetch_alone)):
                mine = [(took, words) for got, took, words in results if got == size]
                if not mine:
                    continue
                times = [took for took, _ in mine]
                kinds: dict[str, int] = {}
                for _, words in mine:
                    kinds[words] = kinds.get(words, 0) + 1
                p99 = quantile(times, 0.99)
                print(f"  class {size} {kind}: {len(mine)}, p50 {quantile(times, 0.5):.3f} s, p99 {p99:.3f} s "
                      f"({p99 / alone[size]:.1f}x alone); "
                      + ", ".join(f"{k} x{v}" for k, v in sorted(kinds.items(), key=lambda kv: -kv[1])), flush=True)
        if not alive or refused * 2 > total or pressed():
            print(f"R={repositories}: stopped ({'daemon gone' if not alive else 'refusal storm' if refused * 2 > total else 'memory pressure'})")
            break
    print(f"load at the end {os.getloadavg()}, free {free_mib():,.0f} MiB")


if __name__ == "__main__":
    main()
