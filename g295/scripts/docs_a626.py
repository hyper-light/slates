import sys
root=sys.argv[1]
def edit(rel, pairs):
    p=root+'/'+rel
    s=open(p).read()
    for old,new in pairs:
        n=s.count(old)
        if n!=1:
            raise SystemExit(f"{rel}: anchor count {n}: {old[:90]!r}")
        s=s.replace(old,new)
    open(p,'w').write(s)
    return s

design=[
# §4.16 verdict sentence
('''Subversion client's commit needs. A `Copy` (A-586) is always judged so, as an addition: its destination's tree must be
absent at the head and unchanged after the base, and its source's tree unchanged after the version it copies.''',
'''Subversion client's commit needs. A `Copy` (A-586) is always judged so, as an addition: its destination's tree must be
absent at the head and unchanged after the base, and its source's tree unchanged after the version it copies. One that
replaces (A-626, `REPLACE`) is judged as its destination tree's removal with that addition: the tree unchanged after the
base and holding no hard link nor any link's target. A copy into another copy's tree in the same increment is judged
against what that copy brings there.'''),
# §4.16 status block
('''> of a green holding a copy, a rebase that restates one, a copy from a version its source changed after, and a copy
> onto a path its own increment removes (the A-586 entry).''',
'''> of a green holding a copy, a rebase that restates one, a copy from a version its source changed after, and a copy
> onto a path its own increment removes (the A-586 entry).
>
> **Status (2026-11-19, A-626 part 1).** Built in the engine: a copy that replaces what its destination holds
> (Subversion's replace with history) and copies nested in one increment, a move inside a copied tree among them (the
> "Copies" bullet below); with them, three defects of A-586's branch made before a copy, which A-626's oracle found.
> Owed (the A-626 entry): a copy from a version its source changed after, a checkpoint of a green holding a copy, and a
> rebase that restates one.'''),
# §4.16 Copies bullet
('''  O(1) a copy and recomputed alike by every holder that applies the same versions. A branch made before a copy cuts the
  copy's tree from its maps (`Ptrie::cut`, a graft's inverse) and takes the copy's entry out of its identity.''',
'''  O(1) a copy and recomputed alike by every holder that applies the same versions. A branch made before a copy cuts the
  copy's tree from its maps (`Ptrie::cut`, a graft's inverse) and takes the copy's entry out of its identity; every key
  the change index names after it is read through the copies above it and set back wherever it differs from what the
  branch then holds (A-626). A copy may replace what its destination holds (A-626, `REPLACE`, Subversion's replace with
  history): the tree is taken away key by key at the copy's version, each removal recorded as removals are, before the
  source is grafted there, and a key's own removal at a copy's version reads as before the copy, so a key the replace
  took away and the copy gave back reads as the copy's. Copies nest in one increment: every source is held before any
  lands (FSFS copies from a revision), they land in their destinations' order, and one inside another's tree is judged
  against what that one brings there.'''),
# §4.17.4 status block
('''> one, a copy from a version its source changed after, and Subversion's replace with history (a copy onto a path its
> own increment removes), each in the A-586 entry.''',
'''> one, a copy from a version its source changed after, and Subversion's replace with history (a copy onto a path its
> own increment removes), each in the A-586 entry.
>
> **Status (2026-11-19, A-626 part 1).** Built: the engine's replace with history (`REPLACE`, a flag on `Copy`: the
> destination's tree taken away key by key at the copy's version, judged as a removal against the base) and copies
> nested in one increment, a second copy into a new branch and a move inside one (a copy and a removal) among them, so
> a stock client's commit holding either is no longer refused. Owed (the A-626 entry): a copy from a version its source
> changed after, a checkpoint of a green holding a copy, and a rebase restating one.'''),
# piece 3
('''   A-586: one record a copy, read by the latest copy above a key, one identity entry a copy; owed from it: the
   checkpoint, the rebase, a copy from a changed source's older version, and the replace with history).''',
'''   A-586: one record a copy, read by the latest copy above a key, one identity entry a copy; the replace with history
   and copies nested in one increment built, A-626 part 1; owed: the checkpoint, the rebase, and a copy from a changed
   source's older version).'''),
# A-586 owed items
('''  - Subversion's replace with history: a copy onto a path its own increment removes, which needs the removed tree held
    by reference.''',
'''  - Subversion's replace with history: a copy onto a path its own increment removes. Built as A-626 part 1, key by key
    rather than by reference (the A-626 entry weighs both).'''),
('''  - moves, links and second copies into a copied tree in one increment.''',
'''  - moves, links and second copies into a copied tree in one increment. Moves and second copies built as A-626 part 1;
    links stay refused (Subversion has none).'''),
]
s=edit('docs/wip/SLATES_DESIGN.md', design)

entry=open(sys.argv[2]).read()
p=root+'/docs/wip/SLATES_DESIGN.md'
s=open(p).read()
if not s.endswith('\n'):
    s+='\n'
s+='\n'+entry
open(p,'w').write(s)

gaps=[
('''> kept whole), a rebase restating a copy (the deriver's `VolumeOp::Copy`), a copy from a version its source changed
> after (refused today), Subversion's replace with history, moves and second copies into a copied tree in one increment,
> the server's change-index consumers treating a copy's destination as its tree (a Merkle identity over the radix trees
> was weighed and declined, A-586).''',
'''> kept whole), a rebase restating a copy (the deriver's `VolumeOp::Copy`), a copy from a version its source changed
> after (refused today), the server's change-index consumers treating a copy's destination as its tree (a Merkle
> identity over the radix trees was weighed and declined, A-586). A-626 part 1 (2026-11-19, g295): Subversion's replace
> with history (`REPLACE`, the destination's tree taken away key by key at the copy's version) and copies nested in one
> increment (a second copy into a copied tree, a move inside one as a copy and a removal) built, with three defects of
> A-586's branch made before a copy fixed; held to a per-version model with a replay and a branch at every version
> (`cargo test -p slates-merge --lib` 99 of 99 on the Mac Studio, red seven ways, see A-626). Owed from A-626: a copy
> from a version its source changed after (part 2: the changes since set back at the destination, recorded in the
> chain entry and reserved before the verdict, with room in the server's entry bound), a checkpoint of a green holding
> a copy (part 3), a rebase restating a copy (part 4); the replaced tree held by reference rather than removed key by
> key, if a measured re-tag's removal outweighs its copy (the A-626 entry).'''),
]
edit('docs/wip/GAPS.md', gaps)
print("ok")
