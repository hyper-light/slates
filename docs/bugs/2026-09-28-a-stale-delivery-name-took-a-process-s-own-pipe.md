# A stale delivery name took a process's own pipe

Date: 2026-09-28. Scope: `crates/ipc/src/delivery.rs` (`take_named`, which `delivered()` and therefore every
`Client::connect` runs), §4.13 / GAP-A9-9. Found as the sibling of the delivery test's false positive
(`docs/bugs/2026-09-28-the-delivery-test-read-a-reused-handle-value-as-the-decoy.md`).

## Symptom

A process that has `SLATES_CONSUMER_FD` in its environment but did not inherit the delivery descriptor, and
has a pipe or socket of its own at the variable's number, loses that descriptor the moment it opens a slates
client. The take adopted the descriptor, marked it close-on-exec, set `O_NONBLOCK` on its open file
description (for every process sharing it), read up to 49 of its bytes, and closed it under its owner. Only
then did it refuse (`WrongLength` or `Corrupt`). On Windows the same happened to a pipe handle at that value:
its inherit flag was cleared, it was peeked, and it was closed.

Who has such an environment: every process a consumer starts. The variable is inherited, and the descriptor
is not — it is closed at the take, and many spawners pass only the standard three (Python's `subprocess`
default `close_fds=True`). So a slates CLI, SDK script or MCP server run by an agent under `slates run` is
exactly such a process. Small numbers are where every process keeps its own pipes and sockets.

Failing test, before the fix (macOS, `cargo test -p slates-ipc --test delivery`): the consumer child takes
its delivery, opens a pipe of its own at the freed number (the kernel's lowest-free rule puts it there),
writes a five-byte probe and asks again:

```
consumer child: a stale take at 11 answered Err(WrongLength { got: 5 }); the pipe there is the same:
false, its status flags kept: false, its probe unread: false
  left: Some(15)
 right: Some(0)
```

## Root cause

`take_named` identified the channel by its number alone, and a descriptor number is only an index into the
current process's table. The earlier incidents were the same fact inside the tests: CI run 36201084174
("IO Safety violation: owned file descriptor already closed"), the 2026-09-14 unit-test pair, and the
Windows decoy check. Each time the test was changed and the take was not.

The obvious identity does not work on macOS. `(st_dev, st_ino)` is unique among *live* pipes, but macOS
reports `st_dev` 0 for every pipe and derives the inode from the pipe's kernel address. So it hands a dead
pipe's pair to the next pipe: 1,999 of 2,000 fresh pipes matched the pipe just closed, at the same number
(scratchpad `pipe_identity.py`, 2026-09-28). Linux never repeated one (0 of 2,000).

## Fix

The delivery name carries the channel's identity, and the take confirms it with calls that touch nothing
before it adopts. Anything that does not match is `NotInherited` and is left exactly as it was.

- **Unix** — `NUMBER:DEVICE:INODE:MTIME:MTIME_NSEC`, from the pipe's `fstat` after the record is written and
  the write end closed. Nothing writes to the pipe again, and a read changes only its access time. On macOS
  the successor pipe's nanosecond modification time is later (2,459 ns in the measured case). On Linux the
  inode never repeats, although the coarse modification time did. The four fields together separate a dead
  pipe from its successor on both.
- **Windows** — `HANDLE:TWIN:PIPE_NAME`. The pipe is now named: `slates-delivery-<pid>-<creation time>-
  <ordinal>`, one instance, `FILE_FLAG_FIRST_PIPE_INSTANCE`, remote clients refused, connected to the
  harness's own client before the name is published anywhere. It is still the same server/client pair
  `CreatePipe` makes, now with a name to read back. The child inherits two handles of its read end. The
  take checks, in this order:
  1. both values are open (`GetHandleInformation`);
  2. they are one object (`CompareObjectHandles`, which reads the handle table and never waits);
  3. it is a pipe;
  4. only then, the pipe's name (`GetFileInformationByHandleEx(FileNameInfo)`).

  The name query is the one call that can wait: on a synchronous handle it waits behind a read pending on
  another thread. The non-blocking pair check keeps it off every handle but the delivery's. The record is
  written in `PIPE_NOWAIT` mode, so a quota that could not hold it shows as a short write, refused, never
  a wait.
- The taxonomy: `NotANumber` is now `Malformed` (a name without its identity, including the old bare
  number, is malformed; no compatibility form is kept). `NotInherited` covers "open, but not the named
  channel".
- A process-tree check (`getppid`, the Windows creator id) was rejected. The intended pass-through flow —
  `slates run -- agent`, whose own `slates mcp` child inherits the untaken descriptor and binds as the
  consumer (`docs/cli.md`) — has a parent that is not the harness. The identity has to be the channel's.

## Tests

- `ipc --test delivery`, the consumer child's check 15 (Unix, by use across real processes): before, as
  above; after, `answered Err(NotInherited); the pipe there is the same: true, its status flags kept:
  true, its probe unread: true` on macOS and on Linux (Docker, kernel 6.12.76-linuxkit).
- `delivery::tests::descriptors::a_name_whose_number_holds_another_channel_leaves_it_untouched` (Unix, in
  process, deterministic): the test's own pipe under another channel's identity is `NotInherited`, with
  its identity, bytes, status and descriptor flags unchanged. The other channel's record still takes whole
  behind its own name.
- `delivery::tests::descriptors::a_malformed_name_and_a_closed_number_are_typed`: eight hostile names,
  including the old bare number, are `Malformed`.
- `ipc --test delivery::a_stale_name_over_this_processs_own_handles_leaves_them_alone` (Windows, both CI
  lanes): the test's own pipe and its twin under a delivery's pipe name is refused by the name; two values
  holding different objects are refused by the pair check. The handles' flags are unchanged and the probe
  is unread.
- The positive path on every platform: the consumer child's take (both Windows lanes run it), the client's
  `--test consumer` 2/2 (macOS 3.2 s, Linux 2.3 s), and the CLI's `run` harness test 1/1.

## Sibling sweep

- The daemon adopts the anchor's NFS listener and fleet serve sockets by number
  (`slates_anchor::ENV_NFS_LISTENER`, `SLATES_ANCHOR_FLEET_SERVE`; `OwnedFd::from_raw_fd` in
  `crates/server/src/daemon.rs` and `fleet.rs`). It is the same pattern, but only the anchor sets those
  variables, only in the daemon it spawns, and no process the daemon starts reads them. Recorded for Ada,
  not changed.
- `crates/mem/src/shared.rs` takes the anchor's segment handoff by number, but it **duplicates** the
  number and never closes the original, so a stale number cannot close a foreign descriptor. It is also
  reached only in the anchor's own child.
