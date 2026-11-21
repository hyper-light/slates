# main <- w-fed-move: (b) named; the fix needs your call on its design (2026-11-21 ~02:50 PST)

## Named (A-803, Slates 081542c5)
The re-anchor in the commit is the step that grows with history. It walks every value of a changed path's history
(A-793's rule: earlier values are pieces of the newest block).
- Stand-in: 1,000 clients, one idle work, release.
- Preload 0, 10^4, 10^5 versions: re-anchor values per verdict 7.9, 30.2, 231.4; judge 24.0, 31.7, 70.9 µs.
- Compose, secure and append stay flat.
- `2c32af2b5` was already in the measured build; it made each value's remap cheap, but every value is still walked.

## Is it the 100k wall? Probably not.
At the 100k rung a path holds a few values (about 31,000 accepts of 4 edits over 42,000 hot files). It is the wall of
a long-lived hot file, where a path's k-th change costs O(k) while any reader pins the floor.

## Design options for the fix (I'd build the first)
1. **An anchor indirection per path.** Earlier values name the path's anchor chunk, whose source is the current block,
   instead of the block itself.
   - A length-preserving change swaps the anchor's block and copies out only the bytes it overwrote, for only the
     pieces that overlap those bytes, found through a per-path interval index of anchor pieces. That is
     O(log + pieces in range) per change, not O(history).
   - A length-changing change, an insert or delete, still re-anchors in full, as today.
   - Agents' edits are mostly overwrites; the load harness's are all overwrites.
   - Risk: a new Source kind, and the index kept exact under fold, seal, checkpoint and recount. An oracle test against
     today's `reanchor` covers it.
2. **Amortized sweeps.** Keep superseded blocks and re-anchor only when the path's stale block bytes reach the file's
   own length. This holds memory at most twice the file and amortizes the CPU over the changes since the last sweep.
   It's simpler, but it trades back some of A-793's memory, and how often it sweeps depends on file size, not count.
3. **Leave it.** It isn't the 100k rung's wall, so wait for r48's `server_counter` rows to name that wall, and do
   whichever comes first.

My recommendation: 3 now, then 1 if r48 doesn't show something bigger. Tell me which.
