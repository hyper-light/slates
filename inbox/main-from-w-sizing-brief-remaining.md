# main <- w-sizing: my brief is not done; what remains and how (2026-11-21)
The person asked me to check my whole assigned goal. Brief items 1 (segment/prefault) and 2's list are done; A-839
(b5249f883) is the first conversion: macOS content slices no longer reserve their arena (139,264 -> 106,496 MiB a
one-shard daemon). What remains, each needing your go given goal 93's focus and your reserve ruling:
- 2b. Image slots (2R a shard), delta log (R), write log (R/4) are fixed ranges of a content object macOS cannot
  resize. Sizing them from the work means slots made as growable objects (as A-789 did for extents), which changes the
  recovery format (A-68), or a proved bound (image <= the shard's table budget) that I have not proved. Plan: slots as
  objects of their own, named per generation, grown by doubling at publication, red first across a restart.
- 3. Device pressure seam: the sampler (A-570) only sets the admission hold. Plan: on a pressure report each shard
  gives back what it can at once (release_idle's free extents, the idle purge) rather than at its next idle pass, and
  admission refuses typed past the hold; red: an injected fall frees a destroyed volume's extents without an idle pass.
- 4. NFSv3 replies: two allocations and one copy of the results per request (results Vec, then reply_record's framed
  Vec); hold_reply and nfs_replay keep a copy too. Plan: v3 results encoded after reserved head room and framed in
  place (as NFSv4 already is), the kept copies keeping that framed buffer; red: an allocation count per READ.
Say go / not now for each. Until then I start nothing (your 11:09 reserve ruling).
