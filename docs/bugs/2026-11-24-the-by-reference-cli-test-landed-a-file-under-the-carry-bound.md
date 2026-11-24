# The by-reference CLI test landed a file under the carry bound (2026-11-24)

## Description
`crates/cli/tests/cli.rs`, `an_owners_unfolded_version_is_read_from_its_content_store_after_a_kill`, failed on CI's
macOS runner (run 45347644597, main 31e0c2596; the first macOS run to reach the Test step since at least 2026-11-19):
"the submit held its increment in the owner's content store", the status naming `merge.inputs.carried_in_entry` (8)
and no `merge.inputs.held_by_owner`. Red again on the MacBook at main 2033268c5.

## Root cause
The test was written for chain entry format 6 (A-825), when every increment was held in its owner shard's content
store. Format 7 (A-854) carries an increment in its chain entry when it is at most the least of
`CARRIED_INCREMENT_MAX` (1 MiB) and a 64th of the partition's green-chain cap (`verbs::carries`), and holds only a
larger one. The test landed a 64 KiB file: under the share on a machine whose derived chain cap is past 4 MiB (CI's
macOS runner, this Mac), so the entry carried it; held only where the cap gives a 64th below 64 KiB. The test assumed
one machine's derived threshold.

## Impact
None on the daemon: carrying a small increment is A-854's design. The test was red on macOS, and since
`cargo test --workspace` stops at a failing binary, every crate after `slates-cli` went unrun on macOS CI.

## Exact edits
- `crates/cli/tests/cli.rs`: the test lands its file in `BYREF_EDIT_BYTES` (64 KiB) edits, a command line's room each
  (macOS's `ARG_MAX` is 1 MiB for every argument and the environment), until it is past
  `slates_server::verbs::CARRIED_INCREMENT_MAX`, so the increment is held on every machine whatever its chain cap;
  the head is read back whole after the kill. Its doc names the carry bound and this failure.
- `crates/server/src/verbs.rs`: `CARRIED_INCREMENT_MAX` is `pub`, so the test reads the daemon's own bound rather than
  repeating it.
