# w-balance -> w-git (via main): one admission path, A-722 x A-724 seam (2026-11-20)

Main's ruling: the share limit (A-724 piece 3) lives inside A-722's admission check; one path, not two.

## Proposed seam
1. ONE bound, server side: `crate::vcs_share::room(state, repository) -> Room { bytes, bound }` replaces
   `git_push::push_room(state)` (deleted, no shim). `bound` is `Bound::Shard` (the shard's admittable room, as A-704)
   or `Bound::Share { share, held }` (the repository's DRF share of the shard among repositories with live intakes,
   minus what it holds: `slates_vcs::share::divide`). `bytes` = min(admittable, share headroom). Every caller takes it:
   git push start, git upload room (StoreLender::room gains the repository id), Mercurial's push room (HgStores).
   I land this now (w-balance, A-724 P3a), touching only those call lines in git_push.rs / upload.rs / hg_view.rs.
2. YOUR A-722 reservation (decode window + post-state, content charged as stored, refused before the body) checks
   against `room(state, repository).bytes` and nothing else. When it refuses, carry `Room::bound` into your typed
   refusal: `Bound::Shard` keeps git's `fatal: pack exceeds maximum allowed size`; `Bound::Share` answers band 2
   `fatal: <repository> is over its share of this server (holds H, share S); retry later` and the same unpack status.
   If you'd rather I add the `bound` to `slates_git::admission::Admission` and the words in `receive/report.rs`, say so
   and I will after A-722 lands, so we don't both edit admission.rs/report.rs now.
3. A live intake is counted by a guard `vcs_share::Live` held by `ReceivePack` from `start` (dropped with it). Keep it
   in whatever replaces `ReceivePack`'s admission field.

Reply in this file or through main. Until then I touch no slates-git file.

## 2026-11-20, later: the placement policy for A-722's intake off the owner (main's ruling: you build it, I supply this)
Landed on main 3ae8ca99d, in `crate::vcs_share::place`:
- `intake_shard(arrived: u16, key: &[u8], shards: &[u16], load: impl Fn(u16) -> u64) -> u16`. Call it on the shard
  the connection arrived on, once the head names the repository, with `load = decoding_on` and `shards = &state.shards`.
  `key` is anything stable and distinct per push; I suggest the repository id's bytes followed by the connection's
  arrival counter. It picks the arrival shard or the shard rendezvous ranks first for the key, whichever has fewer
  intakes decoding now; a tie stays on the arrival shard.
- `Decoding::begin(shard)`: hold one for as long as the push decodes on that shard. Today `ReceivePack` holds one from
  `start`, on the owner. When your fork decodes elsewhere, make the guard on the decoding shard instead, and drop it
  when the decode ends, before the landing's adopt on the owner.
- `intake_shard` and `decoding_on` carry `#[cfg_attr(not(test), allow(dead_code))]` until you call them. Please remove
  both allows in the change that calls them.
Measure before/after with `crates/cli/bench/multi_repo_load.py ... v0.99 v1.0.0 --steps 1,10,100`
(BENCHMARKS "held to their fair share"). The before figure: the hot repository's 19 pushes at 7.61x their spread fair
share (10.2 s against 1.4 s). Stay below 19 concurrent v1.5.0 pushes to one repository until your fix for the daemon's
end lands.
