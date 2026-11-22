# main -> w-git (copy clients, w-sizing): encode_behind holds the git clone's memory; your NEXT item after bug (1) (00:30 PST)
clients' coarse attribution (inbox/main-from-clients-git-memory-coarse.md; big Studio, release a3e673efc, git/git v2.0.0):
- RSS 47 MiB at the start, 917 MiB at the clone's end, then 1,695 MiB 90 s later with NO client connected, still rising
  ~8 MiB/s;
- in those 90 s shard 0 keeps running git_encode/git_derive turns (units_past_budget.encode +3.7 M), A-718's
  encode_behind finishing every version's fragments behind the fetch, held in RAM as content;
- the growth is >= 1 MiB heap blocks, +1.53 GB live, "charged to no budget" (git_encode.rs's own Owed line).

RULED (Ada: "our memory should be far less than that"; banned: unbounded caches; the sizing rule: learned, given back):
- encode_behind's output is derived data. Charge it to the repository's learned derived-data budget (decayed, given
  back when idle). Past that budget, it goes to the store on disk under the repository's standing grant, or is not
  computed ahead at all. No unit runs charged to no budget.
- Nothing grows while no client is connected.
Red first:
- 30 s after a git/git v2.0.0 clone with no client, RSS and the physical footprint are within a derived small multiple
  of the start, not of the history;
- idle encode turns stop once the budget holds.
Order: after bug (1) (the same files), before anything else. Tell clients when you start, so the push-side site table
(its next step) doesn't overlap your files. Main reruns the RSS round when it lands.
