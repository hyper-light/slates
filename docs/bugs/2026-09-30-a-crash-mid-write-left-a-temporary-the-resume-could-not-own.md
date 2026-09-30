# A crash mid-write left a temporary the resume could not own (2026-09-30)

Contract: §4.15 step 11 ("the landing's own temporary or empty fresh directory is removed; anything
else goes back, or is kept"), A-43. Found by CI job 109759894864 (macOS, `baff1ae`) once A-50 moved the
`kill -9` landing test onto a real disk: after the resume, a kept sibling `.slates-kept-…` held zero
bytes, where the test requires a kept sibling to hold a whole round.

## Description

On a filesystem without unnamed temporaries (APFS: no `O_TMPFILE`), a replacement's temporary was
created at the entry's *aside* name, then written, synced, and exchanged with the target. A crash between
creating and finishing the temporary left partial bytes at the aside name.

The resume settles aside names by their manifest entry, and this entry had two recognisers. It was not
the witnessed original, and it did not hash to the overlay (it was incomplete), so step 11's "the landing's
own temporary is removed" never matched. The resume moved it back to the entry's name when that name was
free, or kept it when the name was taken.

## Root cause

The engine recognised its own temporary only by content. A temporary that the crash cut short carries no
recognisable content, and its name was the aside name, which is shared with the displaced original the
exchange leaves there.

## Impact

- **A kept, reported, meaningless sibling** after a crash mid-write. This was observed: a zero-byte
  `.slates-kept-*` on the macOS runner.
- **A torn file at the user's path.** Not observed, but reachable: the resume's put-back moves the entry
  at the aside name to the entry's own name when that name is free, for example after an outsider
  deleted the target between the crash and the resume. That would break step 11's "every written entry
  is old or new, never torn".
- **Linux is unaffected**: its temporaries are unnamed until placed.

## Exact edits (`crates/land/src/engine.rs`)

- **Replacements now build their temporary like creates.** The temporary is created at a plain hidden
  name, which the sweep removes unconditionally as the landing's own, and is written, given its mode and
  mtime, and synced there.
- **Only then does it take the aside name.** It gets `place` (a link) at the aside, and a named
  temporary then leaves its creation name.

A crash leaves one of three states, each with a recogniser that owns it:
- a partial temporary at a plain name, removed by the sweep;
- a complete temporary at the aside, recognised by its overlay hash and removed;
- the displaced original at the aside, recognised by its witness.

Where temporaries are unnamed, the path is unchanged: no extra syscall.

## Evidence

- **Red.** `crates/land/tests/oracle.rs::t_1_15_named_temporaries_crash_at_every_write_instruction_then_resume`,
  and its no-exchange twin, run the crash-at-every-instruction oracle on a simulated filesystem with
  named temporaries (the APFS model; the oracle had only ever run the Linux model). Before the edit:
  "crash 36: siblings swept: [\"/.slates-kept-0000000000000001-0\"]", and crash 38 without the
  exchange.
- **Green.** Every crash point of both, every other `slates-land` test (oracle 22/22), and the OS
  landing tests on APFS.

## Open

- The cost on APFS (one `link` and one `unlink` per replaced file, beside the entry's data `fsync`) is
  not yet measured: the OS rows of `land_bench` build a 10^6-file tree, not run here on a shared machine.
