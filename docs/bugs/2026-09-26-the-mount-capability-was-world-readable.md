# The mount capability was readable by every local user

Date: 2026-09-26. Contracts: §4.13 and AUD-01 (the mount capability is the bearer authority for a
host mount), §4.6. Found while hardening NFSv3 at Ada's direction ("if we do keep NFSv3, then we NEED
to harden it").

## Symptom

A mount's capability, the attachment id and its 16-byte secret token, appeared in three places any
local user can read:

- **`ps` while mounting:** `mount_nfs -o … localhost:/<name>@<attachment>.<token> DIR` carried it on
  its command line.
- **`mount`, permanently:** `localhost:/adcap@1.6621863e353311cbe4d3ed103f8b9fc0 on …`, captured on
  2026-09-26.
- **`nfsstat -m`, permanently:** the "File system locations" list printed the same path.

Every file handle carries that token, and the daemon validates a handle against it. So any local user
who read it could forge handles to the volume and act as that mount.

## Fix (A-34)

- **Mount without the token leaving the CLI** (`crates/cli/src/mount.rs`,
  `crates/bridge-nfs/src/client.rs`, macOS):
  - The CLI asks the daemon's MOUNT service for the root handle itself, over a loopback socket only it
    holds.
  - It calls `mount(2)` with Apple's XDR arguments, the handle in `NFS_MATTR_FH`, so the kernel makes
    no MOUNT call, and the source `slates:/<name>` in `NFS_MATTR_MNTFROM`.
  - The layout is transcribed from `mount_nfs.c`'s `assemble_mount_args` (NFS-343.100.5) and the
    SDK's `<nfs/nfs.h>`.
- **Unmount confirmed by the kernel, not by a secret** (`crates/server/src/nfs.rs`):
  - The kernel's `UMNT` (`NFS_MFLAG_CALLUMNT`) names only `/<name>`, taken from the mount's source.
    Any process could send one, so it proves nothing by itself.
  - The daemon watches every host mount of that volume bound to a mount point, and ends an
    attachment only when the kernel's mount table (`getfsstat(MNT_NOWAIT)`; never a `statfs`, which
    would call into the daemon itself) no longer lists that volume's mount there.
  - A forged `UMNT` of a live mount ends nothing.
- **OCI binds proven by the daemon's record** (`crates/bridge-oci`, `crates/server/src/oci.rs`):
  - The table's source is matched exactly: `slates:/<name>`.
  - The source mount's attachment is the host mount of that volume, for that principal, bound
    (`BindMount`) to exactly that mount point. It is no longer a token read back from the table.

## Evidence

- Live on macOS 26.4.1: `mount` shows `slates:/hard on …` and `nfsstat -m` shows `/nsvol @ localhost`,
  with no capability in either.
- The CLI suite passes 10 of 10. It covers mount, unmount ending the attachment, OCI binds, the
  three-process fleet, and a new forged-UMNT check: the attachment survives three seconds (three times
  the confirmation deadline) after another process's `UMNT`.
- Unit tests: the server's parser reads the CLI's MOUNT call, the server's reply parses to its handle,
  and every truncation is refused. That test found a real gap: a reply cut inside its flavor list had
  been accepted. The mount arguments are also checked to describe their own length.
- Conformance, local: all eight workloads are identical, and fsx passes.

## Edits

- `crates/bridge-nfs/src/client.rs` (new) and `crates/cli/src/mount.rs`, `crates/cli/Cargo.toml`
- `crates/server/src/nfs.rs`, `crates/server/src/oci.rs`
- `crates/bridge-oci/src/{verify,lib,mount_table}.rs`
- tests: `crates/bridge-nfs/tests/client.rs`, `crates/bridge-oci/tests/verify.rs`,
  `crates/cli/tests/cli.rs`
- `unsafe-budget.toml` (slates-cli 5: the `mount(2)` call)
