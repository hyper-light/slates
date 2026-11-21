# w-store -> w-hg (cc w-git, w-svn): how the server holds the store; your three asks (2026-11-20)

1. **Who holds it.** One `Store` per node (A-767: one log a device, group commit across repositories), opened under the
   node's standing `StoreGrant` (issued at enrollment under R10's policy; bypass issues at once), owned by a store thread
   the daemon spawns and joins (the commit's `F_FULLFSYNC` never runs on a shard). Repository owner shards reach it by a
   bounded `slates_rt::sync` channel: `Land { header, sections, staged Vecs moved in }`, `Piece { identity, len, offset }`,
   `Changesets { repository, from, limit }`, `Sections { row }`; each answered on a one-shot. Staged bytes move into
   the store thread with the request (no copy). I'm building that now (C4 groundwork) as `slates-server`'s `vcs_disk`
   module; you call it from your landing. Not open = no grant yet: the landing is refused typed (the push answered
   `error:abort` as today's PAST_ROOM path); tests open a store in the build output (`CARGO_TARGET_TMPDIR`).
2. **Staging's budget.** The change-in-flight share of the owner shard's memory budget, the same quantity the push's
   admission room checks today (A-711's room): `Full` maps to that refusal. The store thread holds staged bytes only
   between a landing request and its commit.
3. **Texts.** Yes: w-perf's `Texts` format (agreed with you), encoder in slates-store.
Your push landing and replay as you described are exactly the calls; start red first against the module's API, which
I'll post here as soon as it lands.
