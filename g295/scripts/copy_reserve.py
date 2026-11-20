import sys
p=sys.argv[1]
s=open(p).read()
def rep(old,new,count=1):
    global s
    n=s.count(old)
    if n!=count:
        raise SystemExit(f"anchor count {n} != {count}: {old[:80]!r}")
    s=s.replace(old,new)
rep('''use super::{
  ConflictWindow, ContentSource, Effect, Green, IDENTITY_TAG_HARDLINK, IDENTITY_TAG_SYMLINK,
  IdentityKey, Increment, Resolved, apply, copy_entry, dir_entry, file_entry, insert_whole,
  link_entry, mode_entry, record, special_entry, union_keys, xattr_entry,
};''','''use super::{
  ConflictWindow, ContentSource, Effect, Green, IDENTITY_TAG_HARDLINK, IDENTITY_TAG_SYMLINK,
  IdentityKey, Increment, Resolved, SPECIAL_NODE_BYTES, apply, change_bytes, copy_entry, dir_entry,
  file_entry, insert_whole, link_entry, mode_entry, record, special_entry, union_keys, xattr_entry,
};''')
rep('''use crate::ops_doc::Op;''','''use crate::ops_doc::{Op, OpKind};''')
rep('''  /// The entries of the copies made at `dst` after `since`: what a copy puts into the set hash (module docs).''','''  /// The destinations of the replaces `inc` declares (A-626), each with what another copy `inc` declares brings to it
  /// first (that copy's source at the destination's place in its tree): a replace takes away the head's tree at its
  /// destination and whatever those copies bring there.
  fn replaces_declared(inc: &Increment) -> Vec<(&str, Vec<String>)> {
    let name = |index: u64| {
      u16::try_from(index)
        .ok()
        .and_then(|index| inc.doc.paths.path(index))
    };
    let copies: Vec<(&str, &str)> = inc
      .doc
      .ops
      .iter()
      .filter(|op| op.kind == OpKind::Copy)
      .filter_map(|op| Some((name(u64::from(op.path))?, name(op.src)?)))
      .collect();
    inc
      .doc
      .ops
      .iter()
      .filter(|op| op.replaces())
      .filter_map(|op| name(u64::from(op.path)))
      .map(|dst| {
        let brought = copies
          .iter()
          .filter_map(|(outer, source)| {
            relative(dst, outer)
              .filter(|rel| !rel.is_empty())
              .map(|rel| joined(source, rel))
          })
          .collect();
        (dst, brought)
      })
      .collect()
  }

  /// [`Green::history_reservation`]'s share for the replaces `inc` declares (A-626): each file the head's tree at a
  /// replaced destination holds keeps its latest value, each IPC node its node. What a copy of the same version brought
  /// there keeps nothing: its values never stood at a version. An upper bound: a replace inside another's tree counts
  /// twice.
  pub(super) fn replace_history_reservation(&self, inc: &Increment) -> usize {
    Green::replaces_declared(inc)
      .into_iter()
      .map(|(dst, _)| {
        let tree = self.tree_keys(dst);
        let nodes = tree.specials.len().saturating_mul(SPECIAL_NODE_BYTES);
        tree
          .files
          .iter()
          .map(|path| self.latest_bytes(path))
          .fold(nodes, usize::saturating_add)
      })
      .fold(0, usize::saturating_add)
  }

  /// [`Green::changes_reservation`]'s share for the replaces `inc` declares (A-626): each path a replace takes away,
  /// named at its destination, at the new version and as its last change.
  pub(super) fn replace_changes_reservation(&self, inc: &Increment) -> usize {
    let mut total = 0usize;
    for (dst, brought) in Green::replaces_declared(inc) {
      let roots = std::iter::once(dst.to_owned()).chain(brought);
      for root in roots {
        let named = |path: &String| {
          let at = joined(dst, relative(path, &root).unwrap_or_default());
          change_bytes(&at).saturating_mul(2)
        };
        let tree = self.tree_keys(&root).paths();
        total = tree.iter().map(named).fold(total, usize::saturating_add);
      }
    }
    total
  }

  /// The entries of the copies made at `dst` after `since`: what a copy puts into the set hash (module docs).''')
open(p,'w').write(s)
print("ok")
