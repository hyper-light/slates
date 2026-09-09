# slates

**Copy-on-write workspaces for coding agents: provisioned in microseconds, kept in RAM, seen as a normal path, merged without guessing, and written to disk only when a human says so.**

[Install](#install) · [Quickstart](#quickstart) · [Merging](#many-agents-one-tree-the-merge-loop) ·
[Landing](#landing-on-disk-under-a-grant) · [MCP](#use-it-with-an-ai-agent-mcp) · [SDKs](#language-sdks) ·
[CLI](#cli-reference) · [Performance](#performance) · [How it works](#how-it-works) ·
[What works today](#what-works-today-and-what-does-not) · [Docs](#documentation)

Slates is a hermetic, in-memory, copy-on-write virtual filesystem service written in Rust. An
agent asks for a volume and gets one in under fifty microseconds. The volume is either scratch
(nothing beneath it) or an overlay over a directory on disk: untouched files are read from disk
on demand, the agent's changes live in memory as exactly the entries that diverged, and the disk
is the source of truth throughout. Ordinary programs (git, cargo, npm, python, editors) see the
volume as a plain path through a kernel mount. Many agents fold their work into one shared
"green" volume through a merge engine that answers accept, identical, or the exact overlapping
bytes, and never invents a merge. Nothing reaches disk until a human grants a landing, and the
landing writes only the manifest the grant was bound to.

It is one binary. The same binary is the supervisor, the daemon, the CLI, and the MCP server, and
the same code path is meant to run on a laptop and across a fleet.

Here is a session on the machine this was written on:

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

*Every console block in this README was captured on 2026-09-09 from the debug build at commit
`dbd63f5` on an Apple M5 Max running macOS 26.4.1, driven by a script that started an anchor,
ran the commands, and killed the process group afterwards. Temporary paths are shortened with
`...` and the MCP block is abridged; nothing else is edited.*

## Status

Slates has no tagged release yet, and this README is careful to separate what runs from what is
designed. As of 2026-09-09:

- **Runs today.** The anchor and daemon, the volume lifecycle (create, snapshot, clone, resize,
  destroy, overlay over a host directory), a real kernel mount on macOS with no privilege and no
  kernel extension, the merge loop (green, work, edit, submit, rebase) on one node with chain
  persistence across a daemon restart, landing plans with grant records and an audit log, an
  MCP server over stdio and loopback HTTP with 23 tools, and synchronous Python and Node SDKs.
- **Does not run yet.** A grant-issuing verb (the CLI prints the command it will be, but the verb
  is absent, so no landing has written a disk through the CLI yet), mounts on Linux and Windows,
  virtio-fs guests and OCI attachments, locked-memory residency behind `--locked`, isolation
  between agents sharing one uid, the async forms of the SDKs, and every fleet feature.

The full list, with the acceptance criterion that closes each item, is the gap ledger's
[§8i](docs/wip/GAPS.md#8i-a-9-contract-correction-and-open-implementation-gaps-2026-09-05).
[What works today](#what-works-today-and-what-does-not) below is the per-platform version.

## Install

There is one executable, `slates`, and it is the supervisor, the daemon, the client, and the MCP
server. Running it needs no Rust toolchain, but until the first tagged release the only way to
get it is to build it.

### From source (Rust 1.98.0)

The toolchain is pinned exactly in `rust-toolchain.toml`; `rustup` picks it up on the first
`cargo` invocation in the checkout.

```sh
git clone git@github.com:hyper-light/slates.git && cd slates
cargo build --release -p slates-cli
mkdir -p "$HOME/.local/bin"
install -m 755 target/release/slates "$HOME/.local/bin/slates"
export PATH="$HOME/.local/bin:$PATH"     # add to your shell profile to keep it
slates --help
```

The release workflow (`.github/workflows/release.yml`) builds the whole workspace in release mode
on nine targets: macOS arm64 and x64; Linux arm64 and x64 on glibc and on static musl; Windows
x64, arm64 and i686. It attaches no binaries yet, so that lane is a build check rather than a
distribution channel. When it does publish, the assets will follow vorpal's one-file-per-platform
convention.

### What a platform gives you

| Platform | Daemon and CLI | Real mount | Notes |
|---|---|---|---|
| macOS 26 (Apple silicon, Intel) | yes | **yes**, `slates mount` over the built-in NFS client; no root, no kernel extension, no Apple entitlement | This is the platform the captured sessions ran on. |
| Linux (glibc, musl) | yes | not yet | The FUSE bridge codec and dispatch exist and are tested; a mounted volume is not yet wired end to end. `slates exec` (the chosen-path launcher) is Linux-only and expects an established root mount. |
| Windows (MSVC) | builds and cross-lints | no | The WinFsp refusal taxonomy is built and tested on every host; the transact half is owed. |

## Quickstart

Start the anchor in one terminal and leave it running:

```sh
slates anchor
```

The anchor measures the machine (page size, cache line, cores, memory, wake latency, memcpy and
hash bandwidth, and so on, which takes a few seconds; `--quick` shortens each probe for tests),
creates one shared-memory segment in RAM with no filesystem entry, publishes the profile into it,
and supervises `slates daemon` as a child. It restarts a daemon that exits, kills one whose
heartbeat lapses, and gives up on a crash loop. `Ctrl-C` stops both. Nothing is written to disk:
there is no data directory, no socket file, no config file.

The instance name is `--instance NAME`, else `SLATES_ENDPOINT`, else `default`. Every command
in another terminal finds the daemon by that name.

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

$ slates volume stat 0001000000000371fd39000000000001
id: 0001000000000371fd39000000000001
name: scratch
referenced_bytes: 0
unique_bytes: 0
lease_epoch: none
attachments: 0
head: 0
snapshots: 1
watcher: scratch
drifted: 0
placed: true
mirror_age_ns: none
host_epoch: 1

$ slates volume create scratch --bounded 512MiB
slates: refused: AlreadyExists { existing: VolumeId { bytes: [0, 1, 0, 0, 0, 0, 3, 113, 253, 57, 0, 0, 0, 0, 0, 1] } }
```

A few things to notice. A snapshot is a number, not a copy: it marks the volume's current epoch,
and later writes copy only the piece they change. A clone is a new volume whose first snapshot
is that number. `referenced_bytes` is what the volume can see; `unique_bytes` is what it alone
holds, which is what a destroy would free. The duplicate name is refused with a typed variant
and exit code 1; every refusal in slates is one of a closed set per subsystem, never a string.

Sizes take a binary unit (`512MiB`, `4GiB`; `B`, `KiB`, `MiB`, `GiB`, `TiB`) or plain bytes.
A `--bounded` volume has its whole quota reserved at creation; `--dynamic MAX` grows in measured
steps against the host's free memory up to the maximum. Ids are 32 hex characters as `create`
prints them.

### Mount it

On macOS a volume becomes a path with one command. Slates serves its own NFSv3 on a loopback
socket held by the anchor, and `mount_nfs` (built into macOS) mounts it at a directory you own.
The `noresvport` option means a high source port, so no root; there is no kernel extension,
no macFUSE, no Apple entitlement, and no `/etc/exports`.

```console
$ MNT=$(mktemp -d)
$ slates mount 0001000000000371fd39000000000001 $MNT
mounted: /private/var/folders/1s/.../T/slates-readme-9_w1qw5m

$ printf 'written through the mount\n' > $MNT/hello.txt && ls -l $MNT && cat $MNT/hello.txt
total 1
-rw-r--r--@ 1 root  wheel  26 Sep  9 12:15 hello.txt
written through the mount

$ slates volume stat 0001000000000371fd39000000000001 | grep bytes
referenced_bytes: 4122
unique_bytes: 4122

$ slates unmount $MNT
unmounted: /private/var/folders/1s/.../T/slates-readme-9_w1qw5m
```

The bytes travel `printf` → the kernel's NFS client → the daemon's NFS server → the owning
shard's volume, and `cat` is a separate process, so the read is a fresh request across the
mount rather than a page-cache echo. The accounting moved: the volume was charged 4,122 bytes
for the write. The listing shows the file as root-owned; each request already runs as the
mounting user, but a created file's owner is not yet taken from that credential, which is an
open item. The root mount model in the design is one kernel
mount per host under which every volume is a directory; the daemon already serves that
synthetic root (`mount /`, `ls /` lists every volume across every shard, `cd <name>` crosses
into one), and `slates mount ID DIR` is the per-volume form of it.

### Overlay a directory on disk

A volume can sit on top of an existing directory. Untouched entries are read from disk on
demand; only what the agent changes lives in memory, together with a *witnessed base* (a stat
fingerprint and content hash of what the disk held when the entry was first changed).

```console
$ slates volume create overlay --bounded 64MiB --base /private/var/.../slates-base-hfwnhbz4
id: 000000000000419328db000000000000
path: (none until a bridge exists)

$ slates base read 000000000000419328db000000000000 /existing.txt
on disk already

$ slates status 000000000000419328db000000000000 --drift
```

`--drift` lists every witnessed entry whose disk copy has changed since it was witnessed (none
here). Drift is reported as an event and is never silently adopted into the agent's view;
`base rewitness` re-reads named entries and `base pin` copies their content into memory so a
later change on disk cannot reach the volume. The base path must be the real directory: a path
through a symlink is refused, because slates never follows or creates one.

## Many agents, one tree: the merge loop

A **green** volume is a shared tree whose only writer is its merge task. It is a numbered chain
of versions. Each agent clones a **work** volume from a version, declares its changes, and
submits an *increment*: a description of the declared operations since its base version, never
a diff of file states and never the bytes themselves. The merge task maps the increment through
everything accepted since that base and returns one of three answers: accepted (with the new
version), identical (someone already made that exact change), or a conflict window naming the
exact overlapping byte range on each side. It does not run diff3, does not ask a model, and
never lets the last writer win.

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

$ slates versions 000000000000058641ce000000000001
head: 1
$ slates changed-since 000000000000058641ce000000000001 0
/notes.txt

$ slates submit 00000000000006087e76000000000003
conflict:
  /notes.txt [0..0] class 4

$ slates submit 00000000000006087e76000000000003 --json
{"accepted":false,"conflicts":[{"at":0,"class":4,"len":0,"path":"/notes.txt"}]}

$ slates rebase 00000000000006087e76000000000003
conflict:
  /notes.txt [0..0] class 4
```

Alice and Bob both created `/notes.txt` at offset 0 from version 0. Alice's submit advanced the
green to version 1. Bob's submit came back as a conflict window at the same range, and so did
his rebase, because the engine cannot move his insert past an overlapping insert it did not
see. Bob resolves it by editing his work volume and submitting again. A third agent whose edit
does not overlap merges over the moved head without any of this:

```console
$ slates work 000000000000058641ce000000000001 carol
id: 000000000000084e3b1a00000000000c
base: 1
$ slates edit 000000000000084e3b1a00000000000c /README.md 0 0 'disjoint file'
edited
$ slates submit 000000000000084e3b1a00000000000c
accepted: 2
```

`edit WORK PATH AT DELETE TEXT` is a splice (delete `DELETE` bytes at `AT`, insert `TEXT`),
which creates the file if it does not exist. The namespace operations (mkdir, rename, unlink,
chmod, symlink, hard link, extended attributes) are declared the same way from the SDKs and the
MCP tools; a mounted work volume will journal them from its filesystem operations. Rebase runs
the same verdict as submit without committing, and when everything maps cleanly it restates the
work's base, content and journal in the head's coordinates so a later disjoint move still
merges. An accepted submit is appended to the green's durable chain in the anchor segment and
replayed after a daemon restart.

The design is §4.16 of the [unified design](docs/wip/SLATES_DESIGN.md); the wiring record with
what is owed (checkpointing, the attachment re-pin, access control on the merge verbs) is
[docs/wip/merge-service.md](docs/wip/merge-service.md).

## Landing on disk, under a grant

The only code in the workspace that writes a host path is the `land` crate, and it writes only
under a grant a human issued for the exact manifest it is about to apply. A landing plans first:
it lists what it would do to disk entry by entry, hashes the plan, and stops.

```console
$ slates land 000000000000419328db000000000000 /private/var/.../slates-land-6f4x83u4
landing: 1
manifest: cd135cf36f2a2030f6592f35f40aeb7023a761e82512614ac82e20027682fd5e
bytes: 0
filtered_out: 0
grant with: slates grant 1

$ slates grants

$ slates audit
0 1119908750 landing_planned grant=None landing=Some(1) outcome=None

$ ls /private/var/.../slates-land-6f4x83u4
```

The plan is recorded, the audit log has one row, and the target directory is untouched. When
the landing runs it takes a single-holder lease on the target, checks each entry's witnessed
base against the disk as it is now, and applies, skips, accepts by identity, or refuses each
one; a file that changed on disk since it was witnessed is a conflict, never an overwrite. The
engine has a crash-injection oracle that kills it at every write instruction and resumes.

Be aware of the current limit: the CLI prints `grant with: slates grant 1`, but the grant verb
is not implemented yet. The server holds grant records and a control channel that is the only
place a grant may be created (the client ring refuses the grant kind, so no agent surface can
mint one), and `--grant N` on `land` consumes an existing grant; issuing one is the next piece
of that path. That is also why no captured session in this README writes a disk.

## Use it with an AI agent (MCP)

`slates mcp` serves the [Model Context Protocol] (revision 2026-07-28) over stdio, or over
loopback Streamable HTTP with `--http PORT`. It is a thin dispatch over the same Rust client the
CLI uses, so an agent drives the same rings and completion records a human does.

Point a client at it, using the binary's absolute path because MCP clients launch servers with
no working directory and often no `PATH`:

**Claude Code**
```sh
claude mcp add slates -- /absolute/path/to/slates mcp --instance default
```

**Claude Desktop, Cursor, and other `mcpServers` clients**
```json
{
  "mcpServers": {
    "slates": { "command": "/absolute/path/to/slates", "args": ["mcp", "--instance", "default"] }
  }
}
```

**Codex CLI**, in `~/.codex/config.toml`:
```toml
[mcp_servers.slates]
command = "/absolute/path/to/slates"
args = ["mcp", "--instance", "default"]
```

The anchor must be running under that instance name. A captured handshake, with JSON-RPC
messages written to the server's stdin:

```console
$ slates mcp
← {"id":1,"jsonrpc":"2.0","result":{"capabilities":{"tools":{}},"protocolVersion":"2026-07-28","serverInfo":{"name":"slates","version":"0.1.0"}}}
← tools/list: 23 tools, 6,299 bytes on the wire
← {"id":3,"result":{"structuredContent":{"green":"0000000000000207d114000000000000"},"content":[...],"isError":false}}
← {"id":4,"result":{"structuredContent":{"volume":"0000000000000209caf0000000000001"},"content":[...],"isError":false}}
← {"id":6,"error":{"code":-32601,"message":"unknown tool: slates.grant"}}
```

The tools: `slates.help`; `slates.volume.{create,list,stat,snapshot,clone,resize,destroy}`;
`slates.merge.{create_green,create_work,edit,declare,submit,rebase,versions,changed_since}`;
`slates.attach.{attach,detach}`; `slates.base.{read_base,rewitness,pin}`;
`slates.land.materialize`; `slates.status`. Every result carries `structuredContent` alongside
the rendered text block, volume ids cross as opaque lowercase hex, a refusal from the daemon is
a JSON-RPC error with the typed message carried through (code `-32000`), and an unreachable
daemon is `-32001`.

There is no grant tool, and there will not be one. `slates.land.materialize` plans the landing
and returns the manifest with `grant_required: true` and the CLI command a human runs:

```json
{"conflicts":[],"grant_required":true,"grant_with":"slates grant 2","landing":2,
 "manifest":"cd135cf36f2a2030f6592f35f40aeb7023a761e82512614ac82e20027682fd5e",
 "summary":{"by_action":[],"bytes":0,"filtered_out":0}}
```

The `--json` flag on the CLI's read and query verbs (`status`, `volume stat`, `volume list`,
`versions`, `changed-since`, `green`, `work`, `edit`, `submit`, `rebase`) emits the same schema
the MCP tools return, and an error under `--json` is a JSON object on stderr with a `kind`. One
definition, two surfaces. Owed on the MCP side: `slates.fs` (reads and writes through the
tool surface rather than a mount), resources and prompts for skills, and `slates mcp install`
to write client configs the way vorpal does.

## Language SDKs

Both SDKs are thin bindings over the typed Rust client, so an agent in Python or Node drives the
daemon through the same rings the CLI uses; there is deliberately no parallel pure-Python or
pure-TypeScript implementation. Both are the synchronous base the design says the async form
wraps; the async forms (fd-readiness futures in Python, Promises in Node) are owed, and neither
package is on PyPI or npm yet.

**Python**, built with [maturin](https://www.maturin.rs) into a `cp39-abi3` wheel:

```sh
maturin build -m crates/sdk-python/Cargo.toml     # target/wheels/slates-*.whl
pip install target/wheels/slates-*.whl
```

```python
import slates

client = slates.Client.connect("default", 5_000_000, 10_000_000)  # reply and reconnect deadlines, ns

green = client.create_green("main")
work = client.create_work(green, "feature")            # {"id": ..., "base": <green version>}
client.edit(work["id"], "/notes.txt", 0, 0, b"hello")   # a splice; creates the file
client.mkdir(work["id"], "/dir")
client.rename(work["id"], "/notes.txt", "/dir/notes.txt")

outcome = client.submit(work["id"])
if outcome["ok"]:
    print("landed at version", outcome["version"])
else:
    for window in outcome["conflicts"]:
        print("conflict at", window["path"], window["at"], window["len"])
```

**Node**, built with [napi-rs](https://napi.rs) and loaded as an addon:

```sh
cargo build -p slates-sdk-node      # target/debug/libslates_sdk_node.dylib (.so / .dll)
cp target/debug/libslates_sdk_node.dylib ./slates.node
```

```js
const { createRequire } = require('node:module');
const slates = createRequire(import.meta.url)('./slates.node');

const client = slates.Client.connect('default', 5_000_000, 10_000_000);
const volume = client.create('scratch', 8 * 1024 * 1024);   // a bounded 8 MiB volume; a hex id
console.log(client.status(volume));                          // { placed, attachments, nfsPort, drifted, … }
client.snapshot(volume);
client.destroy(volume);
```

A connection is pinned to the thread that made it (the rings are single-consumer), so use one
client per thread. Every integer that crosses into JavaScript is range-checked rather than
truncated. Neither SDK has a verb that creates a grant.

→ **[Python SDK](crates/sdk-python/README.md)** · **[Node SDK](crates/sdk-node/README.md)**

## CLI reference

| Command | What it does |
|---|---|
| `slates anchor [--instance NAME] [--quick] [--shards N]` | Measure the machine, own the RAM segment, supervise the daemon |
| `slates daemon [--quick] [--shards N]` | Run the daemon alone (development) |
| `slates profile [--quick] [--json]` | Print the machine profile and every derived constant with its inputs |
| `slates volume create NAME (--bounded SIZE \| --dynamic MAX) [--fold] [--locked] [--base DIR]` | A scratch volume, or an overlay over `DIR` |
| `slates volume list [--json]` · `stat ID [--json]` · `snapshot ID` · `clone ID SNAPSHOT NAME` · `resize ID …` · `destroy ID` · `destroy-snapshot ID SNAPSHOT` | The volume lifecycle |
| `slates volume placed ID [--snapshot N] [--mirror]` | Await a durability scope (the register at f=0 on a laptop) |
| `slates mount ID DIR` · `slates unmount DIR` | A real kernel mount at a directory you own (macOS today) |
| `slates green NAME` · `work GREEN NAME` · `edit WORK PATH AT DELETE TEXT` · `submit WORK` · `rebase WORK` · `versions GREEN` · `changed-since GREEN VERSION` | The merge loop; all take `--json` |
| `slates land ID TARGET [--snapshot N] [--include P] [--exclude P] [--grant N]` · `grants` · `audit [--since N]` | Plan a landing, list grants, read the audit log |
| `slates status [ID] [--drift] [--json]` | The daemon's health per shard, or one volume with its drift list |
| `slates base read ID PATH` · `rewitness ID [PATH …]` · `pin ID [PATH …]` | The overlay's base plane |
| `slates attach ID [--read \| --write] [--snapshot N]` · `detach ATTACHMENT` | Attachment records (not yet a mounted path) |
| `slates exec --volume V --at PATH -- CMD [ARG …]` | Run one command with the volume at a chosen path (Linux; user and mount namespaces, no privilege) |
| `slates mcp [--instance NAME] [--http PORT]` | Serve the MCP tools over stdio or loopback HTTP |

Exit codes: 0 done; 1 the daemon refused (the typed refusal is on stderr); 2 usage; 3 no daemon
at the instance; 4 the command itself failed. Output is one `key: value` per line, one record per
line for lists, `ok` for verbs that return nothing, and raw bytes for `base read`. The full
grammar and its current limitations: **[docs/cli.md](docs/cli.md)**.

## Performance

Every number below is from `docs/wip/BENCHMARKS.md`, which records each one with its command,
hardware, date, and interval; a number without those three does not belong there. All rows are
release builds on an Apple M5 Max (18 cores, 128 GiB, macOS 26.4.1, rustc 1.98.0, mains power),
measured 2026-09-05 unless stated. macOS refuses thread affinity on Apple silicon, so threads are
scheduler-placed and the spread reflects that. Floors ratchet: `cargo xtask ratchet` gates 102
rows against the recorded baseline, and a ceiling is raised only with a dated reason.

### How fast is provisioning?

```
cargo run --release -q -p slates-client --example provision_bench
```

Measured from the Rust client through the real rendezvous and rings against an in-process
daemon, one histogram per concurrency level:

| Clients | p50 | p99 | p999 | Gate |
|---|---:|---:|---:|---|
| 1, spinning | ~9 µs | **~25 µs** | ~31 µs | the 50 µs floor (R9) is held on this p99 |
| 8, spinning | | 34–45 µs | | recorded and ratcheted |
| 64, spinning | | ~2 ms | | informational: oversubscribes the laptop's runnable cores |
| 1, parked | | ~250 µs | | reported separately, as the design asks |

A status round trip (the ring plus the completion record) is p99 ~9 µs. The histogram is its
own lane, not part of the omnibus: run back to back with the microbenches it measured scheduler
contention (a single-client p99 of 25 µs read as 1.5 ms), which is recorded as a lesson in the
benchmarks file.

### What does a volume operation cost?

```
cargo run --release -p slates-vfs --example vfs_bench
```

Trees have the shape of a `cargo build` output tree (36 files per directory, 49-byte names,
measured on this workspace). 95% bootstrap intervals are in the benchmarks file.

| Operation | 10³ files | 10⁵ files | 10⁶ files |
|---|---:|---:|---:|
| Lookup one name in a 36-entry directory | 171 ns | 166 ns | |
| Resolve a three-component path | 312 ns | 333 ns | |
| Readdir of a 36-entry directory | 406 ns | 406 ns | |
| Create a file and unlink it (with the op-log record) | 1,583 ns | 1,500 ns | |
| Write 4 KiB in place · read 4 KiB | 395 ns · 24 ns | | |
| **Snapshot and destroy the snapshot** | 45 ns | 44 ns | **45 ns** |
| Clone and destroy the untouched clone | 265 ns | 260 ns | 244 ns |
| Heap per file (slabs, blocks, names, trie, op log) | 697 B | 472 B | 468 B |
| Build the tree | 1 ms | 107 ms | 1,038 ms |
| Destroy, slice p99 under the shard's step budget | | | 8.2 µs |

A snapshot is forty-five nanoseconds at a million files and does not grow with size, which is
the property the copy-on-write-by-birth-epoch design was chosen for. Destroying a million-file
volume runs in slices the shard schedules between requests, with the allocator out of the
picture (0 ns of the longest slice inside `dealloc`). Seven experiments that lost on the way
(a `BTreeMap` of names, one `Box<str>` per name, object-counted slices, and so on) are on record
in the same file with their numbers.

### The database, the ring, and the machine

| Measurement | Reading | Command |
|---|---:|---|
| One metadata mutation (guard, encode, append, apply) | 206 ns | `slates-db --example db_bench` |
| Recover 10⁴ volumes from 10⁶ log records | 96 ms | same; the budget is 1 s |
| Replay, per record (verify magic, length, sequence, schema, CRC32C; decode; apply) | 95 ns | same |
| One ring round trip, both ends spinning | 278 ns | `slates-ipc --example ipc_bench` |
| One ring round trip, the client parked and woken | 1,051 ns | same |
| Park/unpark wake | p50 2.0 µs, p99 4.6 µs | `slates-machine --example profile` (2026-09-04) |
| Core-to-core ring round trip, all 18 cores | median 153 ns | same |
| BLAKE3, one thread | 2.54 GB/s | same |

The last three rows are the machine profile the anchor measures at boot. Slates carries no
tuning literal: every constant is a formula over anchors like these, logged with its inputs at
start (`slates profile` prints them), and a bare number in a tuning position fails the CI
literal check.

## How it works

```
agent process (SDK / CLI / MCP)             daemon shard S (one pinned core)
──────────────────────────────              ─────────────────────────────────
write a 64-byte request into a ring slot ─▶ poll the ring; read the slot (one cache line)
spin for the measured wake cost, then park  validate; pop a volume record from the slab
                                            insert the name into the shard's radix index
                                            carve the root directory and inode from arenas
                                            charge the quota; reserve the budget
                                            append the op-log record to the anchor segment
                                            publish the new root pointer (release store)
read the reply slot (one cache line)     ◀─ write the reply; signal only if the client parked
```

- **Thread per core, nothing shared.** The daemon is N shards, one per pinned core, each owning
  its volumes, its arena, its content index, its partition of the metadata database, and the
  clients pinned to it. Shards exchange messages on bounded rings; there is no lock on any data
  path and no `Arc` anywhere. The runtime is slates's own (an executor with arena task slots,
  a timing wheel, and one completion seam over kqueue, epoll, io_uring, IOCP, and a
  deterministic simulator); no external async runtime is linked.
- **Copy-on-write by birth epoch.** A snapshot marks the volume's epoch. A write to an older
  node copies just that node; older snapshots keep theirs. Content is chunks in per-shard
  arenas, fingerprinted with BLAKE3 only when sealed, never on the write path.
- **An anchor that outlives the daemon.** A tiny process owns one shared-memory segment (no
  filesystem entry) holding the machine profile, the per-partition operation logs, catalog
  snapshots, and the audit log, and holds the NFS listener. A daemon crash is a restart with the
  metadata, the volume images, and the green chains recovered from that segment; a `kill -9`
  between a verb's effect and its completion record is not a durability hole because the two
  are one log entry.
- **Disk is the source of truth.** An overlay volume reads its base through a read-only seam
  (`openat` with `O_NOFOLLOW`, `getdents`, `pread`); a compile-time lint wall denies `std::fs`
  and every file-creating syscall outside the bridge, IPC, base, and landing crates, and a
  structural test walks the dependency graph to prove a write-capable syscall links only in
  `land`.
- **One VFS operation layer, many transports.** The FUSE, NFSv3, FSKit, and WinFsp bridges each
  turn their own wire into calls on one `Bridge` trait and encode the neutral results back. The
  NFS server on macOS is also the differential oracle for the others.
- **Laptop ≡ fleet.** The metadata register is parameterised by f; a laptop is f=0 of the same
  code, never a branch, and an N=1 differential test asserts identical observable outcomes at
  f=0 and a simulated f=1. Consensus is used only for membership, takeover, and neighbourhood
  changes, never on a write.

The design is one document: **[docs/wip/SLATES_DESIGN.md](docs/wip/SLATES_DESIGN.md)**
(rules R1–R10, decisions D-1…D-27, subsystems §4.1–§4.16, phases 0–9, acceptance criteria and
tests). Its numbered identifiers are cited from code comments, commit messages, and test names.
The evidence behind every decision is under [docs/wip/research/](docs/wip/research/), and each
claim carries an evidence tier (a peer-reviewed paper, a standard or vendor document, deployed
code, a flagged blog post, or a measurement made here).

### The rules, and how each is enforced

| Rule | Enforcement |
|---|---|
| RAM only; disk is written only inside a granted landing | lint wall on `std::fs`/`std::net`; a structural test on the dependency graph; a hermeticity tracer run in the test plan |
| No `Arc`, `Rc`, `Mutex`, `RwLock` | `clippy::disallowed_types` workspace-wide, with the reason in `clippy.toml`; the FFI-edge sites the design allows each carry a comment naming the two owners |
| No magic numbers | every tunable is `derived!("formula", anchors)` from the boot profile; a numeric literal in a tuning position fails `cargo xtask literals` |
| No panics in non-test code | `unwrap`, `expect`, `panic!`, `todo!`, `unreachable!` denied by lint; every refusal is a variant of a closed taxonomy |
| Tests exercise use | no test asserts a file exists or a constant equals; every test drives a mount, a client, MCP, the wire, or the CLI |
| Everything async | thread-per-core with completion drivers; every server and database operation is a future |
| Unsafe is budgeted and only shrinks | `unsafe-budget.toml` per crate, `cargo xtask unsafe`; every block has a `// SAFETY:` line by lint; Miri on the memory, wire, and runtime crates |
| Sub-50 µs provisioning | the histogram above is a permanent, ratcheted CI gate |
| Disk writes need a human grant; no privilege is ever required | grants exist only through the CLI's control channel; MCP and SDKs have no grant verb; the daemon never asks for root or `CAP_SYS_ADMIN` |

## What works today, and what does not

The gap ledger's [§8i](docs/wip/GAPS.md#8i-a-9-contract-correction-and-open-implementation-gaps-2026-09-05)
is the authoritative table (fifteen open rows, each with its acceptance criterion and phase).
The reader's version:

| Area | Today (2026-09-09) | Not yet |
|---|---|---|
| Daemon, anchor, CLI | Anchor supervises and restarts the daemon; every verb in the reference above; `--json` on the read, query, and merge verbs | Scoped names in place of ids; cursors on lists; `--locked` reserving locked memory; bounded and dynamic claims backed by measured host capacity |
| Mounts | macOS: `slates mount` is a real kernel NFS mount with no privilege; the daemon's synthetic root browses every volume across shards; a file round-trips byte for byte | Linux FUSE end to end (its codec and dispatch are tested; the audit found flag and statfs defects to close first); FSKit as the primary macOS path; Windows; the attachment record becoming the mounted path |
| Merge engine | Green, work, edit, declare, submit, rebase, versions, changed-since; per-range content verdicts with a generative oracle; chain persistence across restart; one node | Length-changing overlaps under a conflicting neighbour; checkpointing to bound the chain; the attachment re-pin (`advance`); access control on the merge verbs; cross-shard migration (fleet) |
| Landing | Plans, manifests, grant records, landing leases, audit log; the engine with per-instruction crash injection over the simulated host and tmpfs | A `slates grant` verb and the protected confirmation surface behind it; landings through the CLI have therefore written no disk yet |
| Recovery | Metadata, volume images, and green chains recovered from the anchor segment across a daemon restart | The data-plane content barrier and its process-level proof (mount-blocked); host reboot is out of scope by design |
| Agent surfaces | MCP over stdio and loopback HTTP, 23 tools; synchronous Python and Node SDKs proven against a live daemon | Async SDKs; PyPI and npm packages; `slates.fs`; skills as MCP resources and prompts; `slates mcp install` |
| Security | Per-mount requests run as the mounting user; grants structurally absent from agent channels | Authenticated consumers so two agents on one uid cannot see each other's volumes; the human-approval surface |
| Guests and containers | | virtio-fs devices, OCI attachments, microVMs |
| Fleet | The f-parameterised register with an N=1 differential; the control-plane seal and UDP substrate; TLA+ models of the register and reconfiguration (checked 2026-09-04) | Everything else: membership, takeover, placement, mirroring, the transport's ratification |

Two things a reader should not infer. The differential suite compares the volume core against
tmpfs under a reviewed [equivalence policy](docs/wip/EQUIVALENCE.md) within a generated
operation domain; it does not certify complete POSIX behaviour, native mounts, or guests. And
the merge engine's conflict windows are evidence for an agent to act on, not a merge; slates
never resolves one.

## Documentation

| Doc | What's in it |
|---|---|
| [The `slates` command](docs/cli.md) | Every verb, its output shape, exit codes, and the current limitations |
| [Unified design](docs/wip/SLATES_DESIGN.md) | The whole design: rules, decisions, every subsystem, the phased plan, tests, and benchmarks |
| [Gap ledger](docs/wip/GAPS.md) | What is open, what is owed, the phase records, the model-checking record |
| [Benchmarks](docs/wip/BENCHMARKS.md) | Every recorded measurement with its command, machine, date, and the experiments that lost |
| [Equivalence policy](docs/wip/EQUIVALENCE.md) | What "the same as the host" means in the differential suite, and where the two may differ |
| [Merge service](docs/wip/merge-service.md) · [Recovery](docs/wip/recovery.md) · [Fleet transport](docs/wip/fleet-transport.md) | Per-subsystem wiring records |
| [Research](docs/wip/research/) | The evidence behind each decision: filesystem bridges, CoW structures, IPC and runtimes, databases, replication, merge engines, testing |
| [Python SDK](crates/sdk-python/README.md) · [Node SDK](crates/sdk-node/README.md) | Build, connect, the volume lifecycle, the merge loop |
| [Bug records](docs/bugs/) | Root-cause write-ups; the 2026-09-05 system contract audit is the one to read first |

## Development

```sh
cargo build -p slates-cli                                        # the binary
cargo test --workspace                                            # 809 tests across 26 crates
cargo clippy --workspace --all-targets -- -D warnings             # the lint wall
cargo fmt --all --check
cargo xtask structural                                            # forbidden symbols and syscalls, by dependency graph
cargo xtask literals                                              # every tuning number carries its derivation
cargo xtask unsafe                                                # per-crate unsafe budgets
cargo +nightly miri test -p slates-mem -p slates-wire --lib                       # the runtime's Miri lane is in ci.yml
```

Some suites need the machine to themselves or a RAM-backed directory and skip loudly otherwise:

```sh
SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli -- --test-threads=1           # a real anchor, daemon, and (macOS) kernel mount
cargo test -p slates-vfs --release --test model -- --ignored ac_1_1                  # 10^6 generated operations against the model
SLATES_TEST_RAMDIR=/dev/shm cargo test -p slates-vfs --release --test differential   # against tmpfs (Linux)
SLATES_TEST_RAMDIR=/dev/shm cargo test -p slates-land --release --test os            # landings with kill -9 (Linux)
SLATES_DAEMON=$PWD/target/debug/slates python3 -m unittest discover -s crates/sdk-python/tests
SLATES_DAEMON=$PWD/target/debug/slates SLATES_NODE_ADDON=./slates.node node --test crates/sdk-node/tests/sdk.test.mjs
```

The workspace, in dependency order:

| Crate | What it is |
|---|---|
| `machine` | Boot calibration: the machine profile and the `derived!` macro every tunable goes through |
| `mem` | Pre-faulted locked arenas, per-shard slabs with generational handles, buddy allocation, message-passing frees |
| `rt` | The thread-per-core runtime: executor, timing wheel, inter-shard rings, and one driver seam over kqueue, epoll, io_uring, IOCP, and a simulator |
| `wire`, `wire-derive` | The canonical wire protocol: a 32-byte header checked before allocation, CRC32C, derived encoders, per-class frame caps |
| `vfs` | The copy-on-write volume core: namespace, inodes, content, snapshots, clones, accounting, the journal, the read-only host seam |
| `base` | The real read-only host filesystem behind that seam |
| `land` | The landing engine: manifests, verdicts, leases, the only writer of host paths |
| `merge` | The merge engine: increments, canonical rebase, the position map, the two-pass verdict |
| `archive` | The sealed, self-verifying snapshot stream (also the replication and clone-from-archive format) |
| `db` | The per-shard metadata database: catalog, leases, completion records, grants, audit; the f-parameterised register |
| `anchor` | The anchor process's library: the segment, supervision, restart bounds |
| `ipc` | The client rings, the wake word, and the per-OS rendezvous that hands a region over with no filesystem entry |
| `server` | The daemon: shards, the verbs, the NFS serving path, the landing wiring |
| `client` | The Rust client every other surface speaks through; the provisioning benchmark |
| `bridge-core`, `bridge-fuse`, `bridge-nfs`, `bridge-fskit`, `bridge-winfsp` | One `Bridge` trait and the per-OS transports over it |
| `transport`, `cluster` | The fleet's two planes over UDP and the asynchronous register dispatch (draft) |
| `mcp` | The MCP server as a pure function from a JSON-RPC message to its reply, plus the loopback HTTP transport |
| `sdk-python`, `sdk-node` | The PyO3 and napi-rs bindings |
| `cli` | The `slates` binary: anchor, daemon, verbs, mount, exec, mcp |

Project rules for contributors and for coding agents are in [CLAUDE.md](CLAUDE.md) and
[AGENTS.md](AGENTS.md): the locked rules and their enforcement, the banned list, the code
shape, and the test kinds. The short version is that a change to a subsystem's behaviour
updates its design section, its status blockquote, and the gap ledger in the same commit; a bug
fix starts with a failing test; a benchmark is a recorded command; and a rejected experiment
stays on record with its numbers.

## Acknowledgements

Slates replaces the Go copy-on-write VFS layers of [sylk] and adapts the merge architecture
(green volumes, increments, canonical rebase, the two-pass verdict) from the [hecate]
specifications; both surveys are in the research directory. The runtime, memory, IPC, and
database designs draw on the papers cited inline in the design document, WAFL and EdenFS
among them.

## License

MIT, © 2026 Hyperlight. See [LICENSE](LICENSE).

[Model Context Protocol]: https://modelcontextprotocol.io
[sylk]: https://github.com/hyper-light/sylk
[hecate]: https://github.com/hyper-light/hecate
