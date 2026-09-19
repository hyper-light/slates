# Linux conformance mounts omitted their authority

## Failure and evidence

On 2026-09-19, [conformance job 105965681080](https://github.com/hyper-light/slates/actions/runs/35468726110/job/105965681080)
at `49597a4` failed fsx, fsstress, pjdfstest and workloads before running their workload. Each
Linux `mount.nfs` request returned `No such file or directory`. The harness supplied
`127.0.0.1:/<volume-name>` without an attachment capability.

The minimized regression is
`cargo test -p xtask the_linux_adapter_mounts_with_authority_and_releases_it -- --nocapture`.
On this macOS host, the adapter's old source returned MOUNT status 2 instead of 0 in 0.88 s.
The test drives the real two-shard daemon over NFS, without a privileged kernel mount.

## Cause and impact

AUD-01 correctly requires a mount-owned attachment and its capability (§4.13). The macOS CLI
mount flow was updated; the separate Linux conformance adapter still passed a bare volume name.
The server correctly refused it. Relaxing authorization would restore the security bug.

## Fix

- Have the Linux adapter call `Client::attach_mount`, then pass the returned attachment id and
  token in `/<name>@<attachment-hex>.<token-hex>`.
- Own the attachment until unmount, and detach on a failed mount as well. A kernel UMNT may
  already have removed it; only that typed NotFound is an expected cleanup outcome.
- Redact the capability if the OS mount helper echoes it in an error.
- Keep a socket regression for authorization, stale-capability refusal, attachment teardown,
  abandoned mounts and error redaction.

The same job's hermeticity startup timeout is a separate failure. Its trace contains 8,047
write syscalls before the first daemon kill, mostly repeated kicks to the two shards. That
cause and repair are recorded separately in
`2026-09-19-hermeticity-tracer-stops-unselected-syscalls.md`.

## Validation

The same regression passes after the corrected source: 0.88 s on macOS and 1.13 s in the
Linux arm64 `rust:1.98` container, using the same command above. The full Linux kernel-mount
conformance run has not been repeated locally.
