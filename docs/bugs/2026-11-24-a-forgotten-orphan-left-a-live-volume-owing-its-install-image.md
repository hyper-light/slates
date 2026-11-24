# A forgotten orphan left a live volume owing its install image (2026-11-24, §4.10 "An install image beside the log", A-889)

Found by use: CI's conformance lane, the live-shipped fsstress on `ubuntu-latest`. The step failed in 37 of 55 main runs
from 2026-11-21 06:34 to 2026-11-24 03:16, before and after 21861059f (A-377's amendment, which fixed another way the
lane fell behind). It is a liveness defect. No register invariant was at risk: the owner's log was durable at its head,
one candidate held all of it, and every entry the lagging candidate held was a valid link of the owner's chain.

## Description

fsstress passed its own verdict in every failing run. What failed was the harness's live check: after the 40 s fleet
wait, one candidate held less than the owner's head. Run 45318320246 (`oci-linux`): owner `head=2195 durable=2195
owed=false`, candidate b 2195, candidate c 2149. Earlier runs: 1891 of 2216, 1830 of 2245, 1848 of 2172. Only the FUSE
lanes failed: `native-linux-fuse`, and `oci-linux`, which binds the daemon's FUSE mount into a container. Until
87ef753f3 the records also said "the daemon stopped answering during the run", a misreport of the same event.

## Measured

- adacachy2 (x86_64, 32 cores, load 40 to 55), main e16be6d81: `cargo xtask conformance run --suite fsstress
  --live-ship --transport native-linux-fuse` passed 6 of 6 unpinned. Pinned to 4 CPUs (`taskset -c 12-15`, CI's
  runner has 4), 1 of 8 runs failed, then 1 of 4.
- A diagnostic build logged, once a second, each holder the owner's period left behind, and why. In the failing run,
  for the whole wait: the lagging holder at 1962 of 2287, `pending=install`, `owes_image=true`, no build, no lane,
  catalogued and placed, and `published_whole=false`, the volume unpublished for one reason:
  `changed inodes=[281474976710667] references=[(1, 281474976710667)]`, the roots' `orphans: [281474976710667]` before
  and `orphans: []` now.

## Root cause

The kernel's `FORGET` of a file unlinked while open dropped the file's last lookup after the run's last publication.
`Volume::forget_for` reclaims the orphan at that last reference, which changes the volume: the inode, the attachment's
reference on it, and the roots' orphan list. A `FORGET` has no reply and no barrier, so no publication followed.

The candidate behind the retained tail was owed an install. Its checkpoint was the whole state it already held, so the
volume owed an install image (`LiveVolume::owes_image`), which `install_for` waits for. An image is made right after a
publication is appended, or on the record period for a volume with nothing unpublished (`published_whole`). With the
writer idle, no publication came, and the volume was never clean again, so the image was never made.

A volume left unpublished by a change no verb recorded is a legitimate state: deferred frees already live in it
(A-64, `relieve_deferred`). The fault is the period's assumption that every unpublished volume has a publication on its
way.

## Fix

`crates/server/src/live_ship.rs`: `period_volume` calls `published_for_image` for a volume that owes an image and is
not clean. Unless the volume owes a whole record or its image is already sealing, that publishes the shard
(`verbs::publish_shard`, counted `fleet.live.image_published`, or `fleet.live.image_unpublished` when refused). The
image follows the appended entry in `published`, or the next period once the entry's build ends. A `FORGET` does not
publish itself: a publication per `FORGET` would cost the transport's hot path, and an owed image is rare (main's ruling
of 2026-11-23 21:49).

- Test, red first: `a_holder_behind_the_tail_is_installed_after_a_forget_left_the_volume_unpublished`. On main: "image
  owed true, unpublished true, head 2 before the FORGET; installs sent the holder behind []; entries sent the holder
  ahead []; 0 period publications". With the fix: "installs sent the holder behind [3]; entries sent the holder ahead
  [3]; 1 period publications".
- The lane, pinned to 4 CPUs: 0 of 30 runs failed, against 2 of 12 before. The period's publication was made in 6 of
  the 30 (once each, never refused), the runs that would have stuck.

## Siblings

- Every other change no verb records leaves a volume unpublished the same way, and the same period publication covers
  it, since the rule does not ask what the change was.
- The harness's misreport ("the daemon stopped answering") was fixed by 87ef753f3.
