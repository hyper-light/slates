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

Slates gives each agent its own copy-on-write filesystem in RAM. A volume takes microseconds
to create, shows up as a normal path that git, cargo, npm and editors can use, and can sit on
top of a directory on disk so that untouched files are read from disk and only the agent's
changes live in memory. Agents merge their work into a shared volume through an engine that
returns accept, identical, or the exact bytes that overlap, and nothing is written back to disk
until a person grants it.

It is one binary and one daemon:

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

*The console output in this README was captured from a build at `dbd63f5` on macOS; temporary
paths are shortened.*

Slates has no release yet. The daemon, the CLI, mounts on macOS, the merge engine on one
machine, landing plans, the MCP server and the Python and Node SDKs work today. Linux and
Windows mounts, the grant command that lets a landing write to disk, container and VM guests,
and everything fleet-related are still being built. The [gap ledger](docs/wip/GAPS.md#8i-a-9-contract-correction-and-open-implementation-gaps-2026-09-05)
tracks each item.

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

Mounting a volume works on macOS today, with no root, no kernel extension and no entitlement:
slates serves NFS on a loopback socket and the built-in `mount_nfs` mounts it. On Linux the
daemon and CLI run and the FUSE bridge is tested at the protocol level, but a volume cannot be
mounted yet. On Windows the workspace builds and nothing mounts.

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

A few commands exist but are not finished. `--locked` records the intent but
does not pin memory yet, `attach` records an attachment without mounting anything, and
`exec` (Linux) needs a root mount that the daemon does not establish on its own. The
[CLI guide](docs/cli.md) lists each one.

## Merging

Several agents can work on one tree. A **green** volume is a shared tree with a numbered
history that only the merge engine writes to. Each agent takes a **work** volume from a
version, makes its changes, and submits them. The engine replays the submission against
everything accepted since that version and answers with a new version number, or with the
exact byte ranges that collide. It never guesses at a merge and never lets the last writer win.

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

## Landing

A landing writes a volume's changes into a directory on disk, and it is the only thing in
slates that writes to disk at all. `land` plans it first:

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

The plan lists every file the landing would touch and hashes the list. A grant is tied to that
hash, so what runs is what was approved. When the landing runs, each file is checked against
what the disk held when the volume changed it; a file that changed underneath is refused, not
overwritten. The target must be a real directory, not a path through a symlink.

The `slates grant` command the plan asks for does not exist yet, so nothing has landed through
the CLI so far. When it does exist it will be the one place a grant can be created; the MCP
server and the SDKs have no such verb by design.

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

Add `--instance NAME` if the daemon was started with one. The 23 tools cover volumes
(`slates.volume.*`), the merge loop (`slates.merge.*`), attachments, the base directory
(`slates.base.*`), landings (`slates.land.materialize`), `slates.status` and `slates.help`.
Results come back as `structuredContent`, and a refusal from the daemon is a JSON-RPC error
with its typed name. `slates.land.materialize` plans a landing and returns
`grant_required: true` with the command a person runs; there is no tool that grants.

## Language packages

Both SDKs are thin bindings over the Rust client, so they speak to the daemon exactly as the
CLI does. They are synchronous for now and not yet on PyPI or npm.

**Python**, via [maturin](https://www.maturin.rs):

```sh
maturin build -m crates/sdk-python/Cargo.toml && pip install target/wheels/slates-*.whl
```

```python
import slates

client = slates.Client.connect("default", 5_000_000, 10_000_000)   # deadlines in ns
green = client.create_green("main")
work = client.create_work(green, "feature")
client.edit(work["id"], "/notes.txt", 0, 0, b"hello")
outcome = client.submit(work["id"])
if not outcome["ok"]:
    for w in outcome["conflicts"]:
        print("conflict at", w["path"], w["at"], w["len"])
```

**Node**, via [napi-rs](https://napi.rs):

```sh
cargo build -p slates-sdk-node && cp target/debug/libslates_sdk_node.dylib ./slates.node
```

```js
const slates = require('./slates.node');
const client = slates.Client.connect('default', 5_000_000, 10_000_000);
const volume = client.create('scratch', 8 * 1024 * 1024);
console.log(client.status(volume));
client.destroy(volume);
```

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

The 50 µs p99 for one client is a CI gate. Inside a volume
(`cargo run --release -p slates-vfs --example vfs_bench`, trees shaped like a `cargo build`
output directory):

| Operation | 1,000 files | 1,000,000 files |
|---|---:|---:|
| Look up a name in a 36-entry directory | 171 ns | |
| Create a file and unlink it | 1.6 µs | |
| Write 4 KiB · read 4 KiB | 395 ns · 24 ns | |
| Snapshot | 45 ns | **45 ns** |
| Clone | 265 ns | 244 ns |
| Memory per file | 697 B | 468 B |
| Build the tree | 1 ms | 1.0 s |

A snapshot costs the same at a million files as at a thousand, and destroying a million-file
volume runs in 8 µs slices between other requests. The daemon recovers ten thousand volumes
from a million log records in 96 ms after a restart.

## How it works

```
client                                  daemon shard (one per core)
  write a 64-byte request into a ring ─▶ read it; carve the volume from this shard's arenas
  spin, then park                        charge the quota; append the op-log record to the
                                         anchor's shared segment; publish the new root
  read the reply                      ◀─ reply; wake the client only if it parked
```

- **One shard per core, nothing shared.** Each shard owns its volumes, memory arenas and slice
  of the metadata database. Shards pass messages over bounded rings; there are no locks on the
  data path and no reference counting.
- **Copy-on-write by epoch.** A snapshot marks the current epoch. Writing to an older node
  copies that node; older snapshots keep theirs. Content is hashed only when sealed, never on
  the write path.
- **A supervisor that outlives the daemon.** The anchor holds the operation logs and volume
  images in a shared-memory segment. A daemon crash is a restart with everything recovered
  from that segment.
- **Disk is read, never written.** Overlay volumes read their base through a read-only seam.
  A compile-time lint denies file-writing calls everywhere except the landing crate, and a
  structural test checks the dependency graph to prove it.
- **One code path from laptop to fleet.** The replication protocol is written for f failures
  and a laptop is f = 0 of the same code, tested against a simulated f = 1.

The design is one document, **[docs/wip/SLATES_DESIGN.md](docs/wip/SLATES_DESIGN.md)**, with
every decision and the evidence behind it; the research it draws on is in
[docs/wip/research/](docs/wip/research/).

## Documentation

| Doc | What's in it |
|---|---|
| [CLI guide](docs/cli.md) | Every command, its output, and what is not finished |
| [Python](crates/sdk-python/README.md) · [Node](crates/sdk-node/README.md) | SDK quickstarts |
| [Design](docs/wip/SLATES_DESIGN.md) | Rules, decisions, every subsystem, the build plan |
| [Gap ledger](docs/wip/GAPS.md) | What is open and what closes it |
| [Benchmarks](docs/wip/BENCHMARKS.md) | Every measurement with its command and machine |
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
