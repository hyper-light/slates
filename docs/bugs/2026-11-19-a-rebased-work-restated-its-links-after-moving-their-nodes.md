# A rebased work restated its links after moving their nodes (2026-11-19, found by A-646's sweep, g267; fixed by A-663, g307)

## Description

A clean rebase hands the work its declared operations restated over the head (`Rebased::journal`, §4.16 "Rebase, the
only corrective path"). The work keeps them as its journal, a volume with real inodes would replay them in order, and
the work's next submit composes them again. `Green::rebased_from` restated the verdict's effects in the verdict's own
order: renames, creates and removals first, then the metadata dimensions, hard links among them. A hard link names the
node standing at its target when it is made (A-646), so each link restated after a rename or a removal of its node met
the name afterwards. With the base holding files `f` and `g` and a hard link `h` to `g`:
- `ln g l; mv f g` restated as `mv f g; ln g l`: replayed, `l` named `f`'s file, not `g`'s old one, and the journal
  composed again named the node renamed there (A-646 part 2b's made node), so the green took `l` onto `f`'s bytes too.
- `mv f m; ln m l` and `ln f l; mv f m` both restated as `mv f m; ln f l`: the link named `f`, which the rename had
  emptied, so a volume refused it.
- `ln f l; rm f` restated as `rm f; ln f l`: the link named an emptied name.

Found with it: a link made where the head holds another (`rm h; ln f h`) was restated as `ln f h` alone, with no removal
of the head's `h`. A volume refuses a held name, and so did the deriver composing the rebased work's next submit
(`LinkOverExisting`): the work could never submit again.

## Root cause

The restatement emitted the effects in the order `decide` gathers them, which is the commit's order, and the commit
derives node moves (`Green::node_moves`) that keep a link on its node whatever its place. A journal has no such moves:
each operation acts on names as the operations before it left them. And a retarget's effect is the link alone, since
the deriver nets the work's `rm h; ln f h` into one `Link`, so the restatement never removed the head's link.

## Impact

A rebased work holding a hard link to a node it also renamed, renamed over or removed was handed a journal that a
volume with real inodes could not replay, or that replayed onto the wrong file; resubmitted, the case `ln g l; mv f g`
linked the wrong file in the green. A rebased work that remade a link where the head held one could not submit. No
green was changed by a rebase itself; the wrong link landed only through the resubmitted journal.

## Edits (`crates/merge/src/engine.rs`)

- `in_replay_order`, new: the effects in the order a volume replays them. Each hard link moves to just before the first
  effect that moves its node away from its home or removes it there (`Effect::unhomes`: a rename's source, the file
  its destination held, a removed file or IPC node), when one comes before it, never before the last effect freeing
  the link's own name (`Effect::frees`; `link_places`). Every other effect, and a link nothing moves, keeps the
  verdict's order, which makes a node before it links one.
- `Green::rebased_from` restates the effects in that order, and restates a link made where the head holds a link after
  that link's removal (`Unlink`), unless the effects remove that link themselves (`removed_links`).

## Tests

`crates/merge/src/engine/restate_tests.rs`. Each case is held to four readings of what a reader sees (each name's bytes
and every name of its node), which must agree: the work's own operations in their own order over the head in a model
with real inodes, the green that accepts the work, the restated journal replayed over the head in that model, and the
green that accepts the restated journal composed again (the server's next submit). Five cases, each red first:
`ln g l; mv f g`, `mv f m; ln m l`, `ln f l; mv f m`, `ln f l; rm f`, and `rm h; ln f h`. The oracle generates works over
the same readings, kept to what the deriver composes as a volume replays it, with a census requiring each case.

## Evidence

(Pending the Mac Studio, offline since about 09:20 UTC on 2026-11-19.)

## Siblings found, not changed (GAPS)

- The deriver composes a name freed and made a file again, and a file the work made renamed over a held name, as an
  in-place edit of the name's node, so a hard link naming that node reads the new bytes; POSIX gives the link the old
  file.
- The deriver composes a rename onto a name the work made a hard link at as both the link and the rename, and the green
  holds a file and a link at one name.
- A retargeted symbolic link (`rm s; ln -s t2 s`) is restated as the `Symlink` alone, which the deriver refuses
  (`SymlinkOverExisting`) when the rebased work composes its next submit.
- Directories are restated after the files made in them (`mkdir d; touch d/x` as `touch d/x; mkdir d`), which a
  volume with real directories refuses.
