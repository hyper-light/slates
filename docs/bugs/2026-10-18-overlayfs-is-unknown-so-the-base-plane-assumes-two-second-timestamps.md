# overlayfs is `Unknown` to `fs_kind`, so the base plane assumes 2-second timestamps in containers

- **Date:** 2026-10-18
- **Area:** `crates/base/src/unix.rs` (`fs_kind`), `crates/base/src/lib.rs` (`granularity_for`),
  `crates/vfs/src/base.rs` (the digest/attribute freshness window, §4.5)
- **Status:** found by measurement; **NOT changed** — the value is a safety bound and I cannot prove a
  tighter one for every overlayfs stack. Flagged for Ada with the numbers.
- **Class:** over-conservative default on the deployment the product targets (performance, not correctness)

## Description

The base plane decides how long a digest or attribute may be trusted from the base filesystem's
timestamp granularity: `HostFacts::timestamp_granularity_ns`, which `unix.rs` takes as
`granularity_for(fs_kind(&fs)).max(clock_ns)`. `fs_kind` matches `statfs`'s `f_type`:

| magic | class | granularity |
|---|---|---|
| ext4, XFS, btrfs, tmpfs | `Nanosecond` | 1 ns |
| NTFS | `HundredNanoseconds` | 100 ns |
| HFS+ | `Second` | 1 s |
| FAT, exFAT | `TwoSeconds` | 2 s |
| **anything else** | **`Unknown`** | **2 s** |

**overlayfs — magic `0x794c7630` — is not in the match**, so it takes the `Unknown` arm and the base
plane assumes **two seconds**. overlayfs is the filesystem of every OCI container, which is condition 2
of the goal and the deployment the OCI work targets.

**It is not only overlayfs.** The Linux arm matches exactly eight magics — ext4, XFS, btrfs, tmpfs,
MSDOS, exFAT, NTFS, HFS+ — and everything else falls through. Absent, and so assumed two seconds
(verified by reading the match arms; their *actual* granularities are not measured here, only their
absence):

| absent filesystem | why it matters to slates |
|---|---|
| **overlayfs** | every OCI container — condition 2, and measured at 1 ms below |
| **virtiofs** | slates' own microVM path (`crates/bridge-virtiofs` exists for it) |
| **NFS** | slates' own macOS loopback transport (§4.6 A-34) |
| ZFS, F2FS, bcachefs | ordinary modern server filesystems |
| erofs, squashfs, 9p, CIFS | container images, VM shares, network mounts |

The three in bold are deployment paths slates itself ships. A base directory on any of them gets the
2-second window today.

## The measurement

Inside a Docker container (aarch64, kernel 6.12-linuxkit, `stat -f -c %T` = `overlayfs`, magic
`0x794c7630`), five consecutive writes to the same file:

```
1792337493.999477007
1792337494.004477007
1792337494.005477007
1792337494.006477007
1792337494.007477007
```

The nanosecond field is the constant `…477007` on every write and the values advance in whole
**milliseconds**; two writes inside one millisecond share a timestamp exactly. So the real granularity
there is **1 ms**, against the **2 s** slates assumes — a factor of **2000**.

For contrast, the same probe on the Linux box's btrfs gives true nanosecond spacing
(`…959704950`, `…960212437`, `…960667456`), which `fs_kind` already classifies correctly.

## Impact

Performance, in containers, and only there. A base file's digest cannot be kept until the assumed
window has closed, so on overlayfs a digest computed within two seconds of the file's last write is
discarded as racy and recomputed. The conservatism is **safe** — it is the §4.5 rule that a timestamp
comparison means nothing inside the granularity — and nothing is read stale. It simply does far more
work than the filesystem requires, in exactly the deployment where base-plane reads matter most.

This was invisible until something ran the suite on a container filesystem: every platform CI tests
(ext4 on the Linux runner, APFS on macOS) is classified `Nanosecond`, so the `Unknown` arm never ran.

## Why it is not changed here

The value is an upper bound on how wrong a timestamp may be, so tightening it is a safety change, and
I cannot prove the tighter bound for every overlayfs stack:

1. **Map overlayfs to a new `Millisecond` class.** Matches the measurement, but overlayfs delegates to
   whatever its upper layer is, and `statfs` on the mount does not reveal that layer. The measured 1 ms
   may be this VM's clock rather than overlayfs itself, in which case the class is wrong elsewhere.
2. **Map overlayfs to `Nanosecond`** on the argument that a container upper layer is always a modern
   filesystem. Fastest, and unsafe if that argument is ever false — and it is unverifiable from inside.
3. **Measure the granularity instead of classifying it.** What R3 would want, and what would be right
   for every filesystem at once. Blocked by R1 as the base plane stands: a probe means writing, and the
   base directory is a host path slates only ever reads.
4. **Leave it.** Correct and slow in containers, which is where it is today.

5. **Infer the bound from timestamps slates already reads — no write, so no R1 problem.** The base plane
   already stats every base file it witnesses. Granularity is how finely an mtime can *advance*, so the
   smallest non-zero difference between any two observed mtimes on that filesystem is a sound **upper
   bound** on it: if two files differ by 300 µs, the clock demonstrably advanced within 300 µs, whatever
   the type table says. That tightens only on evidence and never below what was actually observed, so it
   cannot be unsafe the way options 1 and 2 are.

   Two things it is not. It is **not** a proof from a single mtime's precision — a filesystem may store
   nanoseconds and still only update per millisecond, so representation says nothing about advance; only
   a *difference* does. And it is opportunistic: a directory whose files were all unpacked in the same
   second offers no evidence, so the table stays the floor and nothing regresses. The shape is the right
   one either way — a safe default that evidence may tighten, never a guess that evidence must refute.

Options 1 and 2 are guesses. Option 3 generalises but needs a place to write that R1 permits — whether a
granted landing's target, or the daemon's own build area, is a legitimate stand-in for the base's
filesystem (the same mount in some deployments, not in others), which is a design call. **Option 5 looks
like the one that is both sound and already permitted**, and it is the one I would build; it is written
here rather than built because changing how freshness is bounded is Ada's call, not a test fix.

## Owed

Ada's choice among the four. Until then the behaviour is correct and the cost is recorded here.

A related test defect found with it **is** fixed in the same change: `crates/base/tests/host.rs` held a
hardcoded 2 ms `TICK` whose own doc assumed "tmpfs and APFS". It failed 3/3 on overlayfs because the
real window there is the 2 s above. The pause now derives from the same `timestamp_granularity_ns` the
product decides by, so the test is portable and the magic number is gone.
