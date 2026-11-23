# w-git -> main 01a20182: A-850 (bug 2) progress, and one design call (22:55 PST 2026-11-21)

Done, uncommitted in w-git-p2:
- A create's first version brings only its walk beyond the base found at admission (deriving.rs and reach.rs agree).
- Push record v6 carries each move's base; v3, v4 and v5 are refused typed as `Version`, with no shim.
- Replay takes the recorded base through `Repository::based`, and an impossible base is refused typed as
  `StoreRefused::Base`.
- Recovery test `t_a850_a_recovery_replays_each_moves_base_from_its_record` is green: with the record's base removed, the
  replay takes none even though the heads would give one.
- `slates-git --lib -- store` 118/118.

Finding: A-753's finder (the green whose head is the new value, or the commit its tag peels to) covers a tag of a head
commit only. On the tagged line (40 tags of every commit of main), only the tag at main's head gets a base. The other 39
go through the derivation's search back, which isn't recorded, so they still bring their whole history: 2,500 brought
against 160 objects.

What I'm doing unless you stop me: move that search back into admission. Walk back by generation from the tag's commit to
the nearest commit that a committed version's head peels to (an index kept at commit), sliced in admission turns, and
record that version as the base. D(0) is then the tag object plus the commits between, which grows with N, not
N x history (about 280 on the tagged line). Bases stay a function of the log, so reach.rs and replay agree.

Exactly 160 (every tag based on main) would need a reachability test per tag, about N x history commit reads at admission,
so I'm not doing that. Tell me if you want otherwise.

Your kiln socket /Users/adalundhe/.kiln-run/cc-socks/23415.sock is gone (ENOENT), so this is in the inbox.
