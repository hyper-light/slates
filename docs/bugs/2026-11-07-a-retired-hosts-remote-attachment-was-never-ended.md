# A retired host's remote attachment was never ended

**Found** by main CI run 42335884062 on 4326a9d8, job 128292586788 (ubuntu-latest):
`crates/cli/tests/fs_server.rs` `a_retired_hosts_attachments_end_at_the_owner` panicked "still 1 attachments, lease
None, after 40s". It had passed on d61c3be2's runs.

**Reproduced** on adacachy (Linux 7.2.9 x86_64, 2026-11-07) in a capped unit at 2 CPUs with four spinner processes
(`loaded.sh`): 2 of 10, then 1 of 20 runs, each failing one of two ways.
- The client node retired (the owner's members fell from 3 to 2) and its holder's attachment stood 40 s.
- Once, the attachment ended *before* the kill: the owner reported 0 attachments right after the holder attached.

**Root cause**, from the owner's own log lines (a temporary line in the sweep, removed):
1. The sweep was called from two of the nine places that install a configuration (`fleet.rs` `sync_config_from_council`
   and the shards' fan), so the install that removed the retired host often never reached it.
2. It compared the wrong ids. `Consumer::Remote { host }` records the forwarding peer's certificate anchor (what
   `serve_forward` authenticates), and a configuration names members by member id, fresh per incarnation: the owner
   held an attachment of host 2139182739769849067 while the configuration named the client node 8824226135247173624.
   So "not a member" was true of every remote attachment at every install.
3. Ending every host not in the installed configuration also ended a forming host's fresh attachment: a configuration
   installed mid-formation named fewer members than the fleet would.

**Not the cause:** the changes 4326a9d8 added. A formation A/B of d61c3be2 against 4326a9d8's binaries, interleaved,
8 rounds each at 2 CPUs with spinners (three daemons, time until every node probes both peers): 1.2–4.1 s against
1.0–4.3 s, every round formed. The test passed on d61c3be2 because an install that happened to reach the sweep named
neither id, and failed when none did.

**Fix.** `remote_attach::sweep_departed`, on the reap loop's cadence on the control shard: the configuration's
members mapped to their anchors (`member_task::anchor_of`; a member not learned yet maps to none), an anchor among them
at the last look and absent now departed, and every shard ends that anchor's attachments through a call bounded by the
liveness budget, owed again until every shard answered. The test now also holds a live host's attachment for 2.5 s
before the kill (two departure looks), so a sweep that ends every remote attachment fails it.

**Evidence** (adacachy, 2 CPUs and spinners): the retired test 20 of 20, the attachment ending 2.2–4.6 s after the kill;
red with the sweep disabled, 3 of 3; the whole file 10 times serialized under the same load (see the commit).
