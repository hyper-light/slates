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

**The macOS branch has the same shape.** It matches by `f_fstypename` rather than magic, and matches
exactly five names — `apfs`, `hfs`, `msdos`, `exfat`, `ntfs`. Everything else takes `Unknown`, including
**`nfs`** — which is the type of slates' own macOS loopback mount (§4.6 A-34) — and `smbfs`, `webdav`
and `virtiofs`. So the gap is not a Linux accident; both branches enumerate a short list and give
everything else two seconds, and on both platforms the list omits filesystems slates itself produces or
runs on.

**Windows enumerates nothing, and says so.** `windows.rs`'s `facts` returns
`granularity_for(FsKind::Unknown)` unconditionally, with the comment "the coarsest class of the table
until the volume's filesystem is queried through the handle: the racy rule then re-hashes more often,
never less". A deliberate, correctly-reasoned placeholder rather than an oversight — but it means
**every** Windows filesystem gets two seconds, NTFS included, whose own class in this very table is
100 ns. So across the three platforms: Linux classifies 8 filesystems, macOS 5, Windows 0.

That also makes Windows the cheapest place to improve, and the only one where the right answer is not
in doubt: the filesystem name is a `GetVolumeInformation` call the code already anticipates, and
mapping `NTFS` to the class this table already documents for it needs no new judgement. It is not done
here only because this host cannot build or test Windows at all.

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

## Impact — narrower than it first looks

**An earlier draft of this record overstated this and is corrected here.** The rule is relative to the
file's own last change, not to the volume:

```rust
let last_change = fingerprint.mtime_ns.max(fingerprint.ctime_ns);
if self.host.now_ns().saturating_sub(last_change) <= granularity { /* not kept, racy_uncached */ }
```

So the 2-second assumption only withholds the digest of a file changed **within the last two seconds**.
A *static* base — an OCI image layer, a checked-out source tree, anything whose files are minutes or
hours old — is **completely unaffected**: `now - last_change` is far past any granularity, and digests
cache exactly as they do on ext4. The sweeping claim that this costs the OCI deployment generally was
wrong.

Where it does bite is a base directory being **actively written while slates reads it** — an agent
editing the working tree its volume overlays, or a build writing into one. There, every file touched in
the last two seconds has its digest recomputed on each export instead of kept. That is a real scenario
for this product, and it is the one to weigh; it is not the whole container story.

The conservatism itself is **safe** either way — it is the §4.5 rule that a timestamp comparison means
nothing inside the granularity — and nothing is ever read stale. That was checked rather than assumed:
the value has exactly three consumers (`base.rs` 1040, 2252, 2586) and all three ask the same question,
"is this timestamp within granularity of the read, and therefore racy?" — the directory listing's relist
rule, a witness's racy flag, and the digest cache's window. In every one, a **coarser** value classifies
*more* as racy, which costs a relist or a recomputation and can never permit a stale read. Too coarse is
always the safe direction here, which is precisely why tightening it is the one that needs proof.

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

   **Measured, so this is not just an argument.** Taking the smallest non-zero mtime difference over a
   directory's files, on the same container where the table says two seconds:

   | sample | smallest non-zero mtime delta | what option 5 would infer |
   |---|---|---|
   | overlayfs, 40 files written now | **0.001000166 s** | ~1 ms — the measured truth, 2000× tighter than the table |
   | an ordinary checked-out source tree (sampled `.rs` files) | **0.000016928 s** | ~17 µs |

   So the evidence is there in both the written-now case and the ordinary-files case, and in both it
   lands far below the 2 s default. The second row matters most: it says a base directory does not have
   to be freshly written for this to fire — a normal tree of source files already carries it.

   Two things it is not. It is **not** a proof from a single mtime's precision — a filesystem may store
   nanoseconds and still only update per millisecond, so representation says nothing about advance; only
   a *difference* does. And it is opportunistic: a directory whose files were all unpacked in the same
   second offers no evidence, so the table stays the floor and nothing regresses. The shape is the right
   one either way — a safe default that evidence may tighten, never a guess that evidence must refute.

### Option 5 is RETRACTED — it derives a safety bound from untrusted input

I recommended option 5 above and then checked it against this project's own threat model, where it
fails. **An mtime is settable.** `utimensat` is what `tar -p`, `rsync -t`, `cp -p` and `touch -d` all
use, and §4.5's whole base-plane model treats the base directory as a place **outsiders write** —
`SimHost` exists to model exactly that. So the "evidence" option 5 reads is attacker-influenceable: an
outsider can plant two files whose mtimes differ by nanoseconds on a filesystem whose real update
quantum is two seconds, slates infers nanosecond granularity, and then trusts a digest *inside* the
real racy window. Tightening is precisely the unsafe direction, so a wrong inference is not a
performance loss — it is a stale read.

Nor is the obvious patch enough. Restricting it to differences slates itself *witnessed* (file seen at
t1, seen changed at t2) does not help: the new value is still whatever the outsider set it to.

What survives is narrower and worth keeping: the same scan is a sound **diagnostic**. Observing
sub-millisecond spacing where the table claims two seconds is good evidence the table is wrong for this
filesystem, and could be reported — to an operator, or in the boot profile — without being allowed to
move the bound. The measurements above keep their value as exactly that.

### So, standing options

Options 1 and 2 are guesses about a safety bound. Option 4 is correct and slow. **Option 3 — measure by
writing — remains the only sound way to actually tighten it**, and it needs an R1 answer: whether a
granted landing's target, or some location slates may write, can be shown to be the *same filesystem* as
the base (`statfs`'s `f_fsid`/`st_dev` would prove the identity) and may therefore be probed. That is
the design call, and it is Ada's. I am not building any of this while fixing a test.

## Owed

Ada's choice among the four. Until then the behaviour is correct and the cost is recorded here.

A related test defect found with it **is** fixed in the same change: `crates/base/tests/host.rs` held a
hardcoded 2 ms `TICK` whose own doc assumed "tmpfs and APFS". It failed 3/3 on overlayfs because the
real window there is the 2 s above. The pause now derives from the same `timestamp_granularity_ns` the
product decides by, so the test is portable and the magic number is gone.
