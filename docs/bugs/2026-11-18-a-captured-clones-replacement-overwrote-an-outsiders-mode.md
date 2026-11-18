# A captured clone's replacement overwrote an outsider's mode change (2026-11-18)

Contracts: §4.15 step 4's verdict (accept, identical or conflict; D-27: never inferred), A-291 (a captured clone's
landing, judged by content), A-458 (a file's metadata judged field by field), A-262 (the Windows bits a volume keeps).

## Description

A person changed a file's mode on the disk (`chmod 600`, an `attrib +h` on Windows) while an agent, working in a clone of
a captured base, rewrote that file's bytes. Landing the clone wrote the agent's bytes with the clone's mode: the person's
mode change was lost, silently, and the landing reported the entry written. The reverse lost the agent's change: where
the disk already held the agent's new bytes but not the mode the agent had set, the entry was accepted as identical
whole, and the agent's mode never landed. And where both had changed the mode, differently, the clone's won with no
conflict.

An overlay's replacement does none of this: its verdict needs the witnessed file whole (§4.15 step 6, T-1.14), so an
outsider's mode change on a file the agent rewrote refuses the landing (`ModifyModify`) and nothing is overwritten.

Found by g085 as a sibling of A-458 and given as goal 88's next piece (A-466).

## Root cause

A captured clone's entry is judged by content alone (A-291): `verdict::content_replaced` compares the disk's bytes with
what the capture held, and a match is `Apply`. Its witness carried the captured bytes' identity and nothing of the
captured metadata, so nothing could tell a mode the outsider changed from one the agent changed, and the engine staged
every replacement with the clone's mode and bits (`stage_file`: `set_mode(copy.overlay.mode)`,
`set_windows(entry.windows)`). An entry whose bytes the disk already held was `AcceptIdentical`, which writes nothing.

## Impact

Correctness, on every platform: a person's metadata change was overwritten by a landing they granted, without a
conflict shown to them; an agent's mode change on a file an outsider had brought to the same bytes was dropped; and a
mode changed both ways was decided for the clone. Bytes were never at risk: their content verdict was right.

## Exact edits

- `crates/land/src/manifest.rs`: a captured clone's replacement (and A-458's metadata entry) carries the metadata the
  capture held in its content witness (`content_witness_of`), and every content witness encodes it beside its identity,
  so a grant binds what the metadata is judged against (layout `VERSION_CONTENT` 5).
- `crates/land/src/verdict.rs`: `landed_meta(from, disk, to)`, the metadata a replaced file lands with, each field by
  A-458's rule (the volume's where it changed the field and the disk still holds the base's, the disk's otherwise; none
  where the disk holds a third value).
- `crates/land/src/engine.rs`: `Landing::judge_replaced_meta` judges a captured clone's replacement's metadata beside its
  bytes' verdict and records the decision (`ReplacedMeta`, kept across slices): a conflict is `ModifyModify`; bytes to
  write are staged with the decided mode and bits; bytes the disk already holds are kept, and the fields the file lacks
  are set alone through its own handle (A-458's `Landing::chmod`), no byte rewritten.
- Tests: three in `crates/land/src/captured_tests.rs` (the outsider's mode kept under the agent's bytes, a mode both
  changed differently refused, the agent's mode set over bytes already there), and the overlay's sibling in
  `chmod_tests.rs` (refused, the outsider's mode kept).
