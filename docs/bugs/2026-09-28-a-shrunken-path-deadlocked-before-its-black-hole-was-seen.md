# A shrunken path deadlocked before its black hole was seen

Date: 2026-09-28. Design: §4.10a (RFC 8899 §4.3 black-hole detection). Found by
`a_session_finds_its_paths_mtu_and_falls_back_when_the_path_shrinks` (`tests/session.rs`) while path MTU
discovery was being built. It never shipped.

## Evidence

The session found a 9,000-byte path: it confirmed 8,989 bytes after 26 probes, 5 acknowledged and 21 lost.
Then the path shrank to 1,500 bytes, and the next transfer never finished. The test ran past 600 seconds of
wall time and was stopped.

## Root cause

- **Loss detection needs acknowledgements.** After the shrink every packet was framed at 8,989 bytes and
  dropped, so nothing was acknowledged. Packet-threshold and time-threshold loss detection both need an
  acknowledgement of a later packet, so no packet was ever declared lost.
- **So black-hole detection never ran.** It counted only declared losses, so it never counted one.
- **Timeouts only resent the same size.** Each probe timeout sent a copy of the oldest packet, framed for
  the old size, which was dropped too.

## Fix

- **A timeout is evidence.** A probe timeout that fires with packets above the floor in flight counts as one
  loss of an above-floor packet (RFC 8899 §4.3: a timeout is evidence, because a black hole returns
  nothing). At the black-hole threshold the session falls to the floor.
- **The copy is split to fit.** The timeout's copy is split to fit the smaller budget, through the
  retransmission split already built for that fallback: a queued frame larger than the whole packet budget
  is cut at a stream offset, and its tail's offset is recorded as owed at once.

## Test

The same test:
- **Expected:** both 400 KB transfers arrive byte-exact; after the shrink the confirmed size is at most
  1,500 bytes and within 29 bytes of it; at least one black hole is counted.
- **Check:** it hung before the fix and passes after, in 0.04 s of wall time.
