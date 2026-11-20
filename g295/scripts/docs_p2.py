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
design=[
('''absent at the head and unchanged after the base, and its source's tree unchanged after the version it copies. One that
replaces''','''absent at the head and unchanged after the base, and its source's tree must have held something at the version it
copies, a source changed since being copied as it stood then (A-626 part 2). One that
replaces'''),
('''> Owed (the A-626 entry): a copy from a version its source changed after, a checkpoint of a green holding a copy, and a
> rebase that restates one.''','''> Owed (the A-626 entry): a copy from a version its source changed after, a checkpoint of a green holding a copy, and a
> rebase that restates one.
>
> **Status (2026-11-19, A-626 part 2).** Built in the engine: a copy from a version its source changed after, brought as
> the tree stood then (the head's graft fixed by what the change index names since, the fix recorded whole in the chain
> entry, its room derived before the verdict and added to the entry's bound by the service). Owed: a checkpoint of a
> green holding a copy, and a rebase that restates one (the A-626 part 2 entry).'''),
('''  destination, by reference in every map, when the source's tree did not change after that version, so the head's tree
  is the version's.''','''  destination, by reference in every map: the head's tree, which is the version's when the source did not change after
  it, and otherwise is fixed to the version's (A-626 part 2: each key the change index names since set to its value
  then, a copy made since inside the source cut, nothing grafted when one made since replaced it; the fix recorded whole
  in the chain entry, its room derived before the verdict).'''),
('''> a stock client's commit holding either is no longer refused. Owed (the A-626 entry): a copy from a version its source
> changed after, a checkpoint of a green holding a copy, and a rebase restating one.''','''> a stock client's commit holding either is no longer refused. Owed (the A-626 entry): a copy from a version its source
> changed after, a checkpoint of a green holding a copy, and a rebase restating one.
>
> **Status (2026-11-19, A-626 part 2).** Built: a copy from a version its source changed after (`svn cp ^/trunk@N` with
> the trunk changed since, as FSFS takes it), brought as the tree stood then, recorded whole in its chain entry with the
> room it needs derived before the verdict. Owed: a checkpoint of a green holding a copy, and a rebase restating one.'''),
('''   and copies nested in one increment built, A-626 part 1; owed: the checkpoint, the rebase, and a copy from a changed
   source's older version).''','''   and copies nested in one increment built, A-626 part 1; a copy from a changed source's older version built, A-626
   part 2; owed: the checkpoint and the rebase).'''),
('''  - a copy from a version its source changed after (part 2): the source's changes since set back at the destination,
    recorded in the chain entry so replay reads no folded history, reserved before the verdict, with room in the
    server's entry bound (`crates/server/src/verbs.rs`, computed from the increment alone);''','''  - a copy from a version its source changed after (part 2): built as A-626 part 2 (its entry);'''),
]
edit('docs/wip/SLATES_DESIGN.md', design)
p=root+'/docs/wip/SLATES_DESIGN.md'
s=open(p).read()
if not s.endswith('\n'): s+='\n'
s+='\n'+open(sys.argv[2]).read()
open(p,'w').write(s)
gaps=[
('''> (`cargo test -p slates-merge --lib` 99 of 99 on the Mac Studio, red seven ways, see A-626). Owed from A-626: a copy
> from a version its source changed after (part 2: the changes since set back at the destination, recorded in the
> chain entry and reserved before the verdict, with room in the server's entry bound), a checkpoint of a green holding
> a copy (part 3), a rebase restating a copy (part 4); the replaced tree''','''> (`cargo test -p slates-merge --lib` 99 of 99 on the Mac Studio, red seven ways, see A-626). A-626 part 2
> (2026-11-19, g295): a copy from a version its source changed after built, the head's graft fixed by what the change
> index names since, the fix recorded whole in the chain entry (record kind 19) with its room derived before the
> verdict and added to the entry's bound by the service (`cargo test -p slates-merge --lib` 118 of 118 on the Mac
> Studio, red seven ways, see A-626 part 2). Owed from A-626: a checkpoint of a green holding a copy (part 3), a rebase
> restating a copy (part 4); the replaced tree'''),
]
edit('docs/wip/GAPS.md', gaps)
print("ok")
