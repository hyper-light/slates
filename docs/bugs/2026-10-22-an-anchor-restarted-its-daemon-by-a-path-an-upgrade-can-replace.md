# An anchor restarted its daemon by a path, which an upgrade can delete or replace

**Status:** fixed 2026-10-22 (A-145, approved by Ada in plan mode). The mechanism is reproduced deterministically.
Whether it was the trigger of the 2026-10-21 CI tripwire is not shown, and is said so below.
**Severity:**
- An upgrade that replaces the `slates` binary in place leaves every running anchor one daemon crash away from either:
  - the instance ending (the path is gone: `ENOENT`, and the anchor exits);
  - another build running under the old anchor (the path holds the new binary). That is version skew against the
    anchor's segment and rendezvous layouts, which `docs/publish.md`'s one version does not allow.
- The same holds for a daemon the anchor kills itself (a lapsed heartbeat).

**Found:** GAPS' tripwire "an anchor exited during its boot once in seven Linux CLI suite runs" (2026-10-21). Its one
named occurrence printed `slates: start: daemon spawn refused (code Some(2))`.

## What happens

The anchor took its own path once, `std::env::current_exe()` (`crates/cli/src/anchor.rs`), and its supervisor ran
`Command::new(path)` at every start and restart (`crates/anchor/src/supervise.rs`). A path names whatever file is
there now. Once the file the anchor was started from had been deleted or replaced, the next start ran nothing, or ran
something else.

## The evidence

**By hand, on the box** (Linux 7.2.9, 2026-10-22):
1. Start an anchor from a copy of the built binary, and wait for its daemon.
2. Delete the copy. `/proc/<anchor>/exe` then reads `…/slates (deleted)`.
3. `SIGKILL` the daemon.

The anchor printed `slates: step: daemon spawn refused (code Some(2))` and exited. No daemon was left, and the instance
was gone.

**The red test.** `a_daemon_killed_after_its_anchors_executable_is_replaced_restarts_from_the_anchors_own_image`
(`crates/cli/tests/cli.rs`):
1. Start an anchor from a hard link to the built `slates`.
2. Once it serves, replace the link with a script that exits 3.
3. `SIGKILL` the daemon.

Before the fix, on Linux, the anchor restarted the script. It printed `the daemon is in a crash loop (last exit
Some(3))` again and again, backing off 1 s, 2 s, 4 s, 8 s, 16 s, and no successor answered within 20 s.

## Root cause

The supervisor restarted a path, not the image it ran.

## The fix (A-145)

The supervisor runs this process's own executable, and states it: `Supervisor::new(segment, args, policy)` no longer
takes a program.
- **Linux.** Every start execs `/proc/self/exe`, with `argv[0]` set to the path the anchor was started from. The kernel
  resolves that link to the running image's inode even when the path is deleted or replaced.
  - The kernel names a process after the path it was exec'd by, so the daemon would show as `exe` to `ps`, `top` and
    `pgrep NAME`. The daemon now renames itself after its `argv[0]` (`name_this_process`, `PR_SET_NAME`). `argv[0]`,
    and so `pgrep -f`, keep the real path.
- **The other Unix systems (macOS).** These have no such link. The supervisor records the file's device and inode when
  it is made, and checks them before every start.
  - A path that is gone or names another file is refused with `AnchorError::ExecutableReplaced`. The anchor ends saying
    so; restarting it runs the new build.
  - A replacement made between the check and the spawn is not caught, because no portable call spawns a file by its
    descriptor there.
- **Windows.** A running executable cannot be deleted or overwritten there, but it can be renamed aside, and std has
  no stable file identity. The check is owed (GAPS), not guessed.

## What is not shown

The tripwire's one occurrence came at `start` (the first spawn), in the first test after a rebuild, in a container
whose target directory was bind-mounted. Replacing the binary by rename (5 tries) and by cargo's uplift (5 tries)
just before an anchor started did not reproduce it then. So whether a rebuild replaced the path under that anchor is
not shown.

With the fix, the first spawn runs the anchor's own image too, so that trigger, if it was the trigger, is closed as
well.

## Exact edits

- `crates/anchor/src/supervise.rs`: `Supervisor::new` takes no program and returns `Result`. The module `own_image`
  has a paired `identity` and `command` per platform, and the `program` and `image` fields hold what they check.
- `crates/anchor/src/error.rs`: `OwnExecutable { code }` and `ExecutableReplaced { path }`.
- `crates/cli/src/anchor.rs`: the caller.
- `crates/cli/src/daemon.rs`: `name_this_process`, on Linux.
- `crates/anchor/tests/anchor.rs`: six callers.
- `crates/cli/tests/cli.rs`:
  - the test above;
  - `spawn_anchor_from`;
  - `link_and_later_replace`;
  - `forward_lines`, whose thread the `AnchorProcess` joins;
  - the per-platform expectation.
- Docs: `SLATES_DESIGN.md` §2.5 and A-145; GAPS.

## Verification

| | before the fix | after |
|---|---|---|
| Linux, the box | red: a crash loop on the script, no successor within 20 s | green in 0.48 s: the successor's `/proc/PID/exe` is the built binary's inode, and its name is `slates` |
| macOS | red: the same crash loop | green in 3.9 s: the anchor printed `the executable at …/slates was replaced since the anchor started; restart the anchor to run the new build` and ended |

- **`slates-anchor`'s own suite** is green on both platforms. Its cross-process tests supervise the test binary
  through the new spawn:
  - Linux: 12 and 16 passed, 7 ignored;
  - macOS: 12 and 15 passed, 6 ignored.
- **The CLI suite's restart and anchor tests** on the box: 6 of 6 each.
- **Linux gates:** workspace clippy `-D warnings` and the doc gate clean.
- **macOS gates:** workspace clippy `-D warnings`, the doc gate, `cargo xtask check` and `cargo fmt --check` clean.
