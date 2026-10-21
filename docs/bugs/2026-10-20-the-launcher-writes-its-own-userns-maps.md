# `slates exec` writes its own user-namespace maps, which these kernels refuse

> Status: **open**, diagnosed and reproduced on two independent hosts. Found 2026-10-20 by CI's first contact
> with the CLI flow — `crates/cli/tests/cli.rs` gates on `/dev/fuse`, which no machine here has, so the
> launcher had never run anywhere.

## What happens

`the_launcher_refuses_a_path_it_would_have_to_create_and_leaves_the_host_untouched` fails on GitHub's
`ubuntu-latest` with the launcher reporting `slates: open /proc/self/setgroups: code 13` (EACCES) and exiting
4 where 0 was expected.

## Root cause

`exec::enter_namespace` does the textbook in-process sequence (`crates/cli/src/exec.rs`): `unshare(NEWUSER |
NEWNS)`, then write `/proc/self/setgroups` = `deny`, then `gid_map`, then `uid_map`. The `unshare` succeeds —
its error is propagated with `?` and no error is reported — and the **first write** is refused.

Measured on the x86_64 box (Linux 7.2.9), inside a fresh namespace with no maps written:

| | |
|---|---|
| unprivileged user namespaces | **permitted** (`unshare -U true` succeeds) |
| `cat /proc/self/setgroups` | `allow` |
| `cat /proc/self/uid_map` | *empty* |
| `echo deny > /proc/self/setgroups` | **Permission denied** |
| `echo "0 0 1" > /proc/self/uid_map` | **denied** |
| `unshare -U --map-user=$(id -u) --map-group=$(id -g)` | **works**, uid 1000 inside |

So the capability is present and the launcher asks for it the one way these kernels refuse: a process writing
**its own** maps. The idiom that works — and the one `unshare(1)` uses — is for the **parent**, still outside
the new namespace, to write the child's `/proc/<pid>/setgroups` and maps.

That it reproduces identically on a GitHub runner and on a CachyOS box rules out a single host's policy.

## What it is not

It is **not** the AppArmor restriction on unprivileged user namespaces that Ubuntu 24.04 ships. That was the
first diagnosis and it is wrong: `unshare -U true` succeeds on both hosts, so a gate asking that question
never fires, and the launcher fails past it. Recorded because the wrong gate was briefly committed.

## The fix, not yet made

`enter_namespace` has to fork: the child unshares and waits, the parent writes the child's `setgroups`, `gid_map`
and `uid_map`, then releases it to `exec`. That is a restructuring of the launcher's startup with its own error
and lifetime handling — a half-made version is worse than the defect, so it is written down rather than rushed.

Until then `a_process_can_write_its_own_userns_maps` skips the flow loudly and names this file, so the lane
reports the defect once here rather than as a red test every run. `slates exec` is therefore exercised on no
machine, which `docs/wip/GAPS.md` (`8a-launcher-ci`) already records.
