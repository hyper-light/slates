import sys
p=sys.argv[1]
s=open(p).read()
def rep(old,new,count=1):
    global s
    n=s.count(old)
    if n!=count:
        raise SystemExit(f"anchor count {n} != {count}: {old[:80]!r}")
    s=s.replace(old,new)
rep('''//! A path's tree copied by reference in the merge engine (A-586; A-542's second half; §4.17.4 item 3 and piece 3; §4.16
//! "The state's maps"): Subversion's copy, a branch or a tag made from any path at a version, which a stock client's
//! commit declares as an addition with a copy source.''','''//! A path's tree copied by reference in the merge engine (A-586, A-626; A-542's second half; §4.17.4 item 3 and piece
//! 3; §4.16 "The state's maps"): Subversion's copy, a branch or a tag made from any path at a version, which a stock
//! client's commit declares as an addition with a copy source, or as a replacement with one.''')
rep('''//! judges it node-exclusively, an addition: refused when the destination or anything beneath it is present at the head
//! or changed after the increment's base, when the source's tree changed after the version it is copied at, when''','''//! judges it node-exclusively, an addition: refused when the destination or anything beneath it is present at the head
//! (unless the copy replaces it, below) or changed after the increment's base, when the source's tree changed after the
//! version it is copied at, when''')
rep('''//! **Operations on the copied tree in the same increment** (a commit that copies a directory and edits beneath it) are
//! judged against the copied tree, the source's tree at the head, and land after the copy: nothing intervenes in a tree
//! that was absent, so they are checked only for fitting what the copy brings. A file edited there is recorded as a
//! rename's edited destination is (A-547: its source's bytes with the edits spliced in), so replay reads the source as
//! the verdict did. A move, a hard link or a second copy into a copied tree in the same increment is refused, typed
//! (owed).''','''//! **A replace** (A-626, the `REPLACE` flag: Subversion's replace with history, a path its own commit deletes and adds
//! again with a copy source, §4.17.4 item 3.5) takes the destination's tree away before the copy lands. Its verdict
//! judges the removal as an unlink judges one: refused when the tree changed after the increment's base (`DeleteModify`,
//! a delete against a change) or holds a hard link or another link's target (`TypeChanged`: Subversion has none). Its
//! commit removes the tree key by key, each key's history recording its removal at the copy's version (the readers'
//! rule below counts such a removal as before the copy), so every holder answers the tree at the versions before; that
//! costs O(the replaced tree), as removing it key by key costs, and is charged before the verdict
//! ([`Green::replace_history_reservation`], [`Green::replace_changes_reservation`]).
//!
//! **Operations on the copied tree in the same increment** (a commit that copies a directory and edits beneath it) are
//! judged against the copied tree, the source's tree at the head, and land after the copy: nothing intervenes in a tree
//! that was absent, so they are checked only for fitting what the copy brings. A file edited there is recorded as a
//! rename's edited destination is (A-547: its source's bytes with the edits spliced in), so replay reads the source as
//! the verdict did. A second copy into a copied tree (A-626) is judged against what the outer copy brings there, so it
//! needs the replace where that tree holds anything; Subversion's move inside a copied tree, a copy and a removal, lands
//! with it. Every source is held before any copy of the version lands, since FSFS copies from a revision and never from
//! the transaction, and the copies land in their destinations' order, an outer one before those inside its tree. A
//! rename or a hard link with an end in a copied tree is refused, typed: Subversion has neither.''')
rep('''//! A green made at another's version starts from the other's sum at that version, which holds its copies' entries.''','''//! A green made at another's version starts from the other's sum at that version, which holds its copies' entries. A
//! replace's removals move the sum as removals always do; a key a version's copy brought and the same version's inner
//! replace took away gives back the outer copy's entry for it, which its fold reads from the copies above it.''')
open(p,'w').write(s)
print("ok")
