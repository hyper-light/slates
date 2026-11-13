# `slates exec` writes its own user-namespace maps, which these kernels refuse

> Status (2026-11-05, bug-sweep): **re-diagnosed; the refusal is fixed, and no unprivileged route exists on these
> hosts.** The root cause below is wrong: see "Correction (2026-11-05)" at the end. Originally: open, diagnosed and
> reproduced on two independent hosts. Found 2026-10-20 by CI's first contact
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

## Correction (2026-11-05, bug-sweep)

The diagnosis above is wrong, and so is the fix it plans. Both were drawn from a shell probe:
`unshare -U sh -c 'echo deny > /proc/self/setgroups'` execs a shell inside a namespace that has no maps yet, and
that exec'd process is refused its own `/proc/self` files on every kernel. The launcher does not exec before it
writes its maps.

- **The launcher's own sequence works where nothing confines it.** Linux 6.12.76 (linuxkit) in a container, uid 1000,
  2026-11-05:
  - in one process, `unshare(NEWUSER|NEWNS)`, then the self-written `setgroups`, `gid_map` and `uid_map`, then
    `MS_REC|MS_PRIVATE` on `/` and a bind: each step succeeds, with the full capability set inside, and the bind reads
    its marker;
  - the shell probe above is refused on that same kernel;
  - `slates exec` itself ran there for the first time anywhere: the new test's bind path passed.
- **What refuses it on CI's runners is AppArmor.** Diagnostics run 42169647315, on `ubuntu-latest` and
  `ubuntu-24.04-arm` (Linux 7.0.0-1017-azure):
  - `kernel.apparmor_restrict_unprivileged_userns = 1`;
  - `unshare` succeeds, then the kernel logs `apparmor="AUDIT" operation="userns_create" ...
    target="unprivileged_userns"` followed by `apparmor="DENIED" operation="capable" profile="unprivileged_userns"
    capname="sys_admin"`;
  - the in-process sequence is refused at `setgroups` (`EACCES`), `gid_map` and `uid_map` (`EPERM`), the rprivate
    and the bind;
  - `unshare --map-user` and `unshare -Urm` fail too (`write failed /proc/self/uid_map`).

  The profile denies the namespace every capability, whoever writes its maps, so the planned fork, with the parent
  writing the maps, would bind nothing there either. CachyOS (the x86_64 box, unreachable now) was not re-measured.
  That it ships AppArmor's restriction is the likely reading, not checked.
- **What changed:**
  - `crates/cli/src/exec.rs`: a refused map write on a host whose sysctl reads 1 now answers with that setting and
    both ways an administrator can lift it (an AppArmor profile for the binary that grants `userns,`, or the sysctl
    at 0). It used to answer `open /proc/self/setgroups: code 13`. R10 keeps slates from asking for either.
  - `crates/cli/tests/cli.rs`: `the_launcher_binds_a_volume_or_names_the_host_setting_that_forbids_it` needs no
    daemon. It expects that refusal where AppArmor confines, a refusal naming how to enable namespaces where they are
    refused outright, and the command reading the volume through the bind elsewhere.
  - The FUSE flow's gate asks the sysctl now (`the_launcher_can_bind_here`), not the exec'd-shell probe, which had
    skipped it everywhere.
- **Red, then green:** diagnostics branches `diag-bug-sweep-2` (the test over main's `exec.rs`) and `diag-bug-sweep-3`
  (with the fix), recorded in GAPS `8a-launcher-ci`.
