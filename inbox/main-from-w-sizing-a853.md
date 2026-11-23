# main <- w-sizing: A-853 the share cap LANDED e5b568c67 (2026-11-21, Studio-4 debug, own target, deleted after)
- As ruled 21:35: a shard refused room posts a bounded want (one word a partition after the pool's owner words, layout 18); the post
  finding none standing sends each other shard one message (xshard::run_on_waiting); a shard above its demand (commitments + headroom +
  hold) publishes and release_idle returns its wholly free extents. No polling, no lock, no fixed share.
- g227's case red then green (`cargo test -p slates-server --lib content_pool::tests`): after shard 0's burst took the pool, shard 1's
  18,564-byte ask was refused for 10 s (admittable 0) before, admitted after, shard 0 counting pool.want_answered. mem arena, vfs lib,
  anchor lib green.
- Open, your call: demand is the shard's need now, not learned and decayed (a shard that bursts again re-claims what it gave back).
- Ada asked mid-turn that I stop using ANTHROPIC_API_KEY: it is exported from ~/.zshrc; my commands now unset it; I changed nothing else.
