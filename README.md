<p align="center">
  <a href="docs/assets/brand/slates-tablets-preview.png">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="docs/assets/brand/slates-tablets-dark.svg">
      <source media="(prefers-color-scheme: light)" srcset="docs/assets/brand/slates-tablets-light.svg">
      <img src="docs/assets/brand/slates-tablets-light.svg" alt="Slates logo: a working tablet lifted from its source, with a stepped cut in the upper slate" width="90" height="90">
    </picture>
  </a>
</p>

<h1 align="center">slates</h1>
<p align="center"><em>In-memory workspaces for coding agents.</em></p>

Slates gives every agent you run its own copy of your codebase, in memory, in microseconds.
The agent sees a normal directory that git, cargo, npm and its editor all work in. It reads
your real files and its changes stay in RAM, so ten agents on one repository cannot step on
each other or on you. When two of them change the same bytes, slates tells you exactly which
bytes instead of guessing at a merge. And nothing touches your disk until you say so.

Slates does more than isolate work on your laptop. Thousands run agents on remote hosts and
watch them work on your codebase just like they were on your local computer. Agents can:

- Open a snapshot another host just made, without waiting for a copy
- Merge from any host and get the same answer
- Recover work from dead agents without losing anything

All of this made possible from the same binary using the same configuration and same server.

**Merging that never guesses.** Your agents do not hand slates files to merge; they hand it
what they did. Every change on a work volume is recorded as it happens: these bytes at this
offset in this file, this rename, this new directory. When an agent submits, slates replays
that record against everything the others have merged since the agent started, and answers
in one of three ways:

- Merged, and here is the new version
- Already there: someone made the same change
- These exact bytes in this file collide with what someone else landed

A collision comes back as byte ranges, not conflict markers. Nobody's change gets
overwritten. A submission is a list of edits, so it is small and any machine that has the
history can replay it. That is what lets agents in three regions merge into one tree and get
the same answer they would get sharing a laptop. If the machine running a merge dies, a
neighbour that already has the history picks it up and retries whatever was in flight.
Nothing that was accepted is lost.

**Sharding.** Each CPU core owns its own volumes. A request goes to the core that owns the
volume and gets answered there. No locks, nothing shared between cores. That is why creating
a volume takes microseconds and why a busy agent cannot slow down another one. A fleet works
the same way, one host at a time: each volume has one owner and a few neighbours that keep
copies of its snapshots and merges. If agents on another host keep writing to a volume, it
moves there. If the owner dies, a neighbour takes over. If the owner was just paused, it
cannot come back and clobber what the neighbour did. The only thing the cluster ever votes on
is who owns what.

Slates consists of a single binary and a single daemon:

```console
$ slates volume create scratch --bounded 512MiB
id: 0001000000000371fd39000000000001
path: (none until a bridge exists)

$ MNT=$(mktemp -d) && slates mount 0001000000000371fd39000000000001 $MNT
mounted: /private/var/folders/1s/.../T/slates-readme-9_w1qw5m

$ printf 'written through the mount\n' > $MNT/hello.txt && cat $MNT/hello.txt
written through the mount

$ mount | grep slates-readme
localhost:/scratch on /private/var/folders/1s/.../T/slates-readme-9_w1qw5m (nfs, nodev, nosuid, mounted by adalundhe)

$ slates unmount $MNT
unmounted: /private/var/folders/1s/.../T/slates-readme-9_w1qw5m
```

> [!NOTE]
> The console output in this README was captured from a build at `dbd63f5` on macOS;
> temporary paths are shortened.

> [!IMPORTANT]
> Slates has no release yet. The daemon, the CLI, mounts on macOS, the merge engine on one
> machine, landing plans, the MCP server and the async Python and Node SDKs work today. Linux
> and Windows mounts, the grant command that lets a landing write to disk, container and VM
> guests, and the fleet (whose protocol core and transport exist, but not yet the wiring between
> nodes) are still being built. The [gap ledger](docs/wip/GAPS.md#8i-a-9-contract-correction-and-open-implementation-gaps-2026-09-05)
> tracks each item.

## Install

Build from source with Rust 1.98 (the toolchain file pins it, so `rustup` picks it up):

```sh
git clone git@github.com:hyper-light/slates.git && cd slates
cargo build --release -p slates-cli
sudo mv target/release/slates /usr/local/bin/   # or add target/release to PATH
slates --help
```

Release builds for macOS, Linux (glibc and musl) and Windows run in CI on every tag, but no
binaries are attached yet.

> [!IMPORTANT]
> Mounting a volume works on macOS today, with no root, no kernel extension and no
> entitlement: slates serves NFS on a loopback socket and the built-in `mount_nfs` mounts it.
> On Linux the daemon and CLI run and the FUSE bridge is tested at the protocol level, but a
> volume cannot be mounted yet. On Windows the workspace builds and nothing mounts.

## Quickstart

Start the daemon in one terminal and leave it running:

```sh
slates anchor
```

`anchor` measures the machine (page size, cache line, wake latency, memcpy and hash bandwidth,
and so on), keeps one shared-memory segment in RAM, and supervises the daemon as a child,
restarting it if it dies. `Ctrl-C` stops both. There is no data directory, socket file or
config file.

In another terminal:

```console
$ slates volume create scratch --bounded 512MiB
id: 0001000000000371fd39000000000001
path: (none until a bridge exists)

$ slates volume list
0001000000000371fd39000000000001 scratch referenced=0 unique=0 overlay=false

$ slates volume snapshot 0001000000000371fd39000000000001
snapshot: 0

$ slates volume clone 0001000000000371fd39000000000001 0 copy
id: 000100000000043df000000000000004

$ MNT=$(mktemp -d) && slates mount 0001000000000371fd39000000000001 $MNT
mounted: /private/var/folders/1s/.../T/slates-readme-9_w1qw5m
```

A snapshot is a number, not a copy; later writes copy only the pieces they change. A clone is
a new volume that starts from a snapshot. `--bounded` reserves the whole quota up front;
`--dynamic MAX` grows as needed up to the maximum. Sizes take `B`, `KiB`, `MiB`, `GiB` or
`TiB`.

To work on top of an existing directory, pass it as the base. Files you have not touched are
read from disk; files you change are kept in memory along with a record of what the disk held
when you changed them, so a later change on disk is reported rather than silently picked up:

```console
$ slates volume create work --bounded 4GiB --base /Users/you/project
$ slates base read <id> /Cargo.toml          # read a file straight from the base
$ slates status <id> --drift                 # base files that changed since you touched them
```

> [!WARNING]
> A few commands exist but are not finished. `--locked` records the intent but does not pin
> memory yet, `attach` records an attachment without mounting anything, and `exec` (Linux)
> needs a root mount that the daemon does not establish on its own. The [CLI guide](docs/cli.md)
> lists each one.

## Merging

Put ten agents on one codebase and you need their work to come back together without anyone
quietly losing an edit. In slates you give them a shared **green** volume; each agent takes its
own **work** volume from it, changes what it likes, and submits. You get one of three answers:
it merged and here is the new version, it was already there, or these exact bytes collide with
what someone else landed. Nobody's change is ever overwritten and slates never invents a merge
for you.

```console
$ slates green main
id: 000000000000058641ce000000000001

$ slates work 000000000000058641ce000000000001 alice
id: 00000000000005c5ae4b000000000002
base: 0
$ slates work 000000000000058641ce000000000001 bob
id: 00000000000006087e76000000000003
base: 0

$ slates edit 00000000000005c5ae4b000000000002 /notes.txt 0 0 'alice was here'
edited
$ slates edit 00000000000006087e76000000000003 /notes.txt 0 0 'bob was here'
edited

$ slates submit 00000000000005c5ae4b000000000002
accepted: 1

$ slates submit 00000000000006087e76000000000003
conflict:
  /notes.txt [0..0] class 4

$ slates submit 00000000000006087e76000000000003 --json
{"accepted":false,"conflicts":[{"at":0,"class":4,"len":0,"path":"/notes.txt"}]}
```

Alice's submit moved the tree to version 1. Bob wrote the same bytes of the same file, so his
comes back as a conflict at that range; he fixes his copy and submits again. An edit that does
not overlap merges over the moved head without any of that:

```console
$ slates work 000000000000058641ce000000000001 carol
id: 000000000000084e3b1a00000000000c
base: 1
$ slates edit 000000000000084e3b1a00000000000c /README.md 0 0 'disjoint file'
edited
$ slates submit 000000000000084e3b1a00000000000c
accepted: 2
```

`edit WORK PATH AT DELETE TEXT` deletes `DELETE` bytes at offset `AT` and inserts `TEXT`,
creating the file if needed. `rebase WORK` moves a work volume onto the current head without
submitting. `versions GREEN` and `changed-since GREEN VERSION` read the history. Directory
operations (mkdir, rename, unlink, chmod, symlinks, links, xattrs) are declared from the SDKs
and MCP tools. Accepted submissions survive a daemon restart.

What makes this work at scale is that a submission never carries bytes. An **increment** is a
fixed-size description of the operations an agent declared since its base version, naming the
sealed result by hash. The engine maps those ranges through the exact deltas accepted since
that version (a position map that composes, so a work volume a thousand versions behind costs
the same per operation as one that is current), and the verdict is a pure function of the
increment and the chain. Because it is pure, any machine holding the chain can recompute it.

In a fleet that is exactly what happens. A green volume has one owner, which runs its merge
task. An agent on any host submits by routing the green's id to that owner; the work volume's
sealed result is placed on its holders first, then the merge record goes out as the next entry
of the green's replicated ledger, to all of its candidate holders, and commits when f+1 have
it on hand and every hash the new version references is already placed. Each holder recomputes
the verdict and the manifest before serving the version and refuses on a mismatch. If the owner
dies, the holder that already has the ledger takes over and in-flight submissions retry by
identity, so no accepted version is ever lost and no conflict is ever decided twice. A team of
agents in three regions merging into one tree gets the same accept, identical or conflict answer
they would get on one laptop, from the same code.

## Landing

When you want an agent's work on your real disk, you land it. That is the only way anything in
slates ever writes to disk, and it happens in two steps so you can see what you are approving.
`land` shows you the plan first:

```console
$ slates land 000000000000419328db000000000000 /private/var/.../slates-land-6f4x83u4
landing: 1
manifest: cd135cf36f2a2030f6592f35f40aeb7023a761e82512614ac82e20027682fd5e
bytes: 0
filtered_out: 0
grant with: slates grant 1

$ slates audit
0 1119908750 landing_planned grant=None landing=Some(1) outcome=None
```

You see every file the landing would touch, and your grant is tied to that exact list, so what
runs is what you approved and nothing more. If you edited a file yourself after the agent
changed it, that file is refused rather than overwritten. The target must be a real directory,
not a path through a symlink.

> [!WARNING]
> The `slates grant` command the plan asks for does not exist yet, so nothing has landed
> through the CLI so far. When it does, it will be the only place a grant can come from: an
> agent cannot grant itself disk access through MCP or an SDK, because those surfaces have no
> such verb.

## Use it with an AI agent (MCP)

`slates mcp` is a [Model Context Protocol] server over stdio (or loopback HTTP with
`--http PORT`). Start `slates anchor` first, then point your client at the binary by absolute
path:

**Claude Code**
```sh
claude mcp add slates -- /usr/local/bin/slates mcp
```

**Claude Desktop, Cursor and other `mcpServers` clients**
```json
{
  "mcpServers": {
    "slates": { "command": "/usr/local/bin/slates", "args": ["mcp"] }
  }
}
```

**Codex CLI**, in `~/.codex/config.toml`:
```toml
[mcp_servers.slates]
command = "/usr/local/bin/slates"
args = ["mcp"]
```

> [!TIP]
> Use the binary's absolute path: MCP clients start servers with no working directory and
> often no `PATH`. Add `--instance NAME` if the daemon was started with one.

The 23 tools cover volumes
(`slates.volume.*`), the merge loop (`slates.merge.*`), attachments, the base directory
(`slates.base.*`), landings (`slates.land.materialize`), `slates.status` and `slates.help`.
Results come back as `structuredContent`, and a refusal from the daemon is a JSON-RPC error
with its typed name. `slates.land.materialize` plans a landing and returns
`grant_required: true` with the command a person runs; there is no tool that grants.

## Language packages

Both SDKs are thin bindings over the Rust client, so an agent in Python or Node speaks to the
daemon over the same rings the CLI uses. Both are async first: every verb is awaitable and is
resolved by the daemon's completion descriptor on your own event loop (`asyncio` in Python,
libuv in Node), with no extra runtime or thread underneath. A blocking `Client` with the same
verbs is there for scripts.

> [!NOTE]
> The packages are `slates` on PyPI and `@hyper-light/slates` on npm. Neither is published
> yet, so for now build them from the checkout as shown.

**Python** (`pip install slates`; from a checkout, `maturin build -m crates/sdk-python/Cargo.toml`
then `pip install target/wheels/slates-*.whl`):

```python
import asyncio
import slates

async def main():
    client = slates.AsyncClient.connect("default", 5_000_000, 10_000_000)  # deadlines in ns

    # provision a volume; every verb is one await, resolved on the asyncio loop
    volume = await client.create("scratch", 8 * 1024 * 1024)
    print(await client.status(volume))

    # many agents' worth of volumes at once: each reply routes to its own await
    ids = await asyncio.gather(*(client.create(f"agent-{n}", 8 * 1024 * 1024) for n in range(8)))

    # the merge loop
    green = await client.create_green("main")
    work = await client.create_work(green, "feature")
    await client.edit(work["id"], "/notes.txt", 0, 0, b"hello")
    await client.mkdir(work["id"], "/dir")
    outcome = await client.submit(work["id"])
    if outcome["ok"]:
        print("merged as version", outcome["version"])
    else:
        for w in outcome["conflicts"]:
            print("conflict at", w["path"], w["at"], w["len"])
        # fix the work volume, then: await client.rebase(work["id"]) and submit again

asyncio.run(main())
```

**Node / TypeScript** (`npm install @hyper-light/slates`, Node 18 or later; a prebuilt addon
per platform, types included):

```ts
import { AsyncClient } from '@hyper-light/slates';

const client = AsyncClient.connect('default', 5_000_000, 10_000_000);

const volume = await client.create('scratch', 8 * 1024 * 1024);
console.log(await client.status(volume));

const ids = await Promise.all([...Array(8)].map((_, n) => client.create(`agent-${n}`, 8 * 1024 * 1024)));

const green = await client.createGreen('main');
const work = await client.createWork(green, 'feature');
await client.edit(work.id, '/notes.txt', 0, 0, Buffer.from('hello'));
const outcome = await client.submit(work.id);
if (!outcome.ok) {
  for (const w of outcome.conflicts) console.log('conflict at', w.path, w.at, w.len);
}
```

Every verb in the CLI table below has an async method with the same name (camelCase in Node),
including `land`, which plans a landing and returns `grant_required` like the CLI does. A
refusal from the daemon is a typed `SlatesError` in Python and an `Error` in Node, and an
integer that would not survive the trip into a JavaScript number is refused rather than
rounded.

→ **[Python quickstart](crates/sdk-python/README.md)** · **[Node quickstart](crates/sdk-node/README.md)**

## CLI reference

| Command | What it does |
|---|---|
| `slates anchor [--quick] [--shards N]` | Measure the machine and run the daemon under supervision |
| `slates volume create NAME (--bounded SIZE \| --dynamic MAX) [--base DIR]` | A new volume, empty or over a directory |
| `slates volume list \| stat ID \| snapshot ID \| clone ID SNAP NAME \| resize ID … \| destroy ID` | The volume lifecycle |
| `slates mount ID DIR` · `slates unmount DIR` | Mount a volume at a directory you own (macOS) |
| `slates green NAME` · `work GREEN NAME` · `edit WORK PATH AT DELETE TEXT` · `submit WORK` · `rebase WORK` · `versions GREEN` · `changed-since GREEN VERSION` | Merging |
| `slates land ID DIR [--snapshot N] [--include P] [--exclude P] [--grant N]` · `grants` · `audit` | Landing |
| `slates status [ID] [--drift]` | Daemon health, or one volume and its drifted base files |
| `slates base read ID PATH` · `rewitness ID [PATH …]` · `pin ID [PATH …]` | The base directory under an overlay |
| `slates profile` | The machine profile and the constants derived from it |
| `slates mcp [--http PORT]` | The MCP server |

`--json` on the read, query and merge verbs gives the same shape the MCP tools return. Exit
codes: 0 ok, 1 refused (the reason is on stderr), 2 usage, 3 no daemon, 4 the command failed.
Every command with its output: **[docs/cli.md](docs/cli.md)**.

## Performance

Release builds on an Apple M5 Max (18 cores, 128 GB, macOS 26.4.1, rustc 1.98.0), measured
2026-09-05. Commands, intervals and the experiments that lost are in
[`docs/wip/BENCHMARKS.md`](docs/wip/BENCHMARKS.md).

Creating a volume, measured from a client process through the real rendezvous and rings
(`cargo run --release -p slates-client --example provision_bench`):

| | p50 | p99 | p999 |
|---|---:|---:|---:|
| One client | 9 µs | **25 µs** | 31 µs |
| Eight clients | | 34–45 µs | |

The 50 µs p99 for one client is a CI gate. A snapshot costs the same at a million files as
at a thousand, destroying a million-file volume runs in 8 µs slices between other requests,
and the daemon recovers ten thousand volumes from a million log records in 96 ms after a
restart.

<details>
<summary>Inside a volume (<code>cargo run --release -p slates-vfs --example vfs_bench</code>, trees shaped like a <code>cargo build</code> output directory)</summary>

| Operation | 1,000 files | 1,000,000 files |
|---|---:|---:|
| Look up a name in a 36-entry directory | 171 ns | |
| Create a file and unlink it | 1.6 µs | |
| Write 4 KiB · read 4 KiB | 395 ns · 24 ns | |
| Snapshot | 45 ns | **45 ns** |
| Clone | 265 ns | 244 ns |
| Memory per file | 697 B | 468 B |
| Build the tree | 1 ms | 1.0 s |

</details>

## How it works

```
client                                  daemon shard (one per core)
  write a 64-byte request into a ring ─▶ read it; carve the volume from this shard's arenas
  spin, then park                        charge the quota; append the op-log record to the
                                         anchor's shared segment; publish the new root
  read the reply                      ◀─ reply; wake the client only if it parked
```

- **Why it is fast.** Each CPU core runs one shard that owns its volumes outright, so a request
  never waits on a lock. Your agent drops a request in a ring and usually has the answer before
  it would have finished going to sleep.
- **Why snapshots are free.** A snapshot is a number. Writing after it copies only the piece
  being changed, and the old snapshot keeps the old piece. Hashing happens when something is
  sealed, never while an agent is writing.
- **Why a crash costs you nothing.** A small supervisor process holds every volume's state in a
  shared-memory segment that the daemon does not own. If the daemon dies it is restarted and
  picks everything back up from that segment.
- **Why your disk is safe.** The code that reads your directory cannot write to it: the only
  crate allowed to write a host path is the landing engine, and a compile-time check fails the
  build if anything else links a file-writing call.
- **Why a fleet is not a different product.** The replication code is written for f failures.
  Your laptop runs it with f = 0, and the test suite runs the same code at a simulated f = 1
  and checks that the answers match.

The design is one document, **[docs/wip/SLATES_DESIGN.md](docs/wip/SLATES_DESIGN.md)**, with
every decision and the evidence behind it; the research it draws on is in
[docs/wip/research/](docs/wip/research/).

## From a laptop to a fleet

If you run agents on more than one machine, this is the part of slates for you. It is sized
for a monorepo like Meta's or Google's: a billion files, millions open per mount, thousands of
hosts, agents in several regions. On a fleet your agents can:

- **Open any snapshot from any host, right away.** The agent sees the whole tree the moment
  the snapshot exists; only the files it reads are fetched, and they are checked against their
  hash on the way in. After a few runs slates knows which files a build touches first and has
  them waiting.
- **Lose a machine without losing work.** Everything an agent has snapshotted or merged is
  already in the memory of a few neighbouring machines in different failure domains. When the
  machine dies, a neighbour that has it all takes over. A machine that was only paused cannot
  come back and undo its successor's work.
- **Pick how safe each write needs to be.** Most writes take the fast path. When a result
  matters, the agent asks for that one write to be placed in the region, or mirrored to another
  region, and waits only for that. Every reply tells you whether it is placed and how far behind
  the mirror is. Lose a whole region and the mirror takes over.
- **Write without waiting on a cluster.** The only thing the cluster ever votes on is who is a
  member and who takes over a dead host. A write is one round of copies to the neighbours and
  nothing else.
- **Have their volume follow them.** A volume lives on the host that created it. If an agent
  on another host keeps writing to it, it moves there. Load alone never moves it.
- **Clone an overlay across hosts and still see the real directory.** Untouched files keep
  coming from the original directory; once you have captured a base, every remote clone shares
  that capture by hash.

```console
$ slates volume placed 00000000000006f0ec41000000000000
placed: true
mirror_age_ns: none

$ slates volume placed 00000000000006f0ec41000000000000 --mirror
slates: refused: Unsupported { feature: "mirror" }
```

On your laptop the region is placed as soon as the local append lands, and the mirror is
refused because there is nobody to mirror to. The fields are the ones a fleet fills in.

> [!NOTE]
> What you can run today is the one-machine case of all of this. The replication core runs at
> f=0 and at a simulated f=1 with a test that asserts identical outcomes, the takeover and
> reconfiguration protocols were model-checked on 2026-09-04, and the node-to-node transport
> runs end to end on a simulated network. Joining real machines together is Phase 8, and there
> is no fleet benchmark yet. The design is §4.8 and §4.10 of the
[unified design](docs/wip/SLATES_DESIGN.md) and the [transport draft](docs/wip/fleet-transport.md);
the reading behind it, from EdenFS and Piper to Vertical Paxos and copysets, is in
[docs/wip/research/](docs/wip/research/).

## Documentation

| Doc | What's in it |
|---|---|
| [CLI guide](docs/cli.md) | Every command, its output, and what is not finished |
| [Python](crates/sdk-python/README.md) · [Node](crates/sdk-node/README.md) | SDK quickstarts |
| [Design](docs/wip/SLATES_DESIGN.md) | Rules, decisions, every subsystem, the build plan |
| [Gap ledger](docs/wip/GAPS.md) | What is open and what closes it |
| [Benchmarks](docs/wip/BENCHMARKS.md) | Every measurement with its command and machine |
| [Fleet transport](docs/wip/fleet-transport.md) · [Merge service](docs/wip/merge-service.md) | The node-to-node planes; how the merge engine is wired |
| [Equivalence policy](docs/wip/EQUIVALENCE.md) | Where a volume is allowed to differ from the host filesystem |

## Contributing / development

```sh
cargo build -p slates-cli
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask check          # forbidden calls, tuning literals, unsafe budgets
```

The CLI, mount and SDK suites start a real daemon and need the machine to themselves:

```sh
SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli -- --test-threads=1
SLATES_DAEMON=$PWD/target/debug/slates python3 -m unittest discover -s crates/sdk-python/tests
```

Project rules, the workspace layout and the test kinds are in [CLAUDE.md](CLAUDE.md) and
[AGENTS.md](AGENTS.md).

## Acknowledgements

Slates replaces the Go copy-on-write filesystem in [sylk] and adapts the merge design from the
[hecate] specifications.

## License

MIT — © 2026 Hyperlight. See [LICENSE](LICENSE).

[Model Context Protocol]: https://modelcontextprotocol.io
[sylk]: https://github.com/hyper-light/sylk
[hecate]: https://github.com/hyper-light/hecate
