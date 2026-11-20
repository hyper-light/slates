import sys
p=sys.argv[1]; scratch=sys.argv[2]
s=open(p).read()
def rep(old,new,count=1):
    global s
    n=s.count(old)
    if n!=count:
        raise SystemExit(f"anchor count {n} != {count}: {old[:80]!r}")
    s=s.replace(old,new)
# module doc
rep('''//! The engine's `Copy` (A-586; A-542's second half; §4.17.4 item 3 and piece 3): a tree copied by reference reads as a
//! copy at every version, is recorded once, costs its paths and never its tree, is refused typed where Subversion's
//! commit would not take it, lands with the edits its own increment makes beneath it, replays from its chain entry, is
//! taken away by a branch made before it, and keeps the identity equal to its recount. The oracle holds a green to a
//! reference green that copies key by key, through the engine's plain operations, over generated histories.''',
'''//! The engine's `Copy` (A-586, A-626; A-542's second half; §4.17.4 item 3 and piece 3): a tree copied by reference reads
//! as a copy at every version, is recorded once, costs its paths and never its tree, is refused typed where Subversion's
//! commit would not take it, lands with the edits its own increment makes beneath it, replaces what its destination
//! holds when it says so, nests with the other copies of its increment, replays from its chain entry, is taken away by a
//! branch made before it, and keeps the identity equal to its recount. The oracle holds a green to a model that states
//! every version's tree by the design's rules, over generated histories.''')
rep('''use std::collections::BTreeSet;''','''use std::collections::{BTreeMap, BTreeSet};''')
rep('''use crate::ops_doc::{NODE_EXCLUSIVE, Op, OpKind, OpsDoc};''','''use crate::ops_doc::{NODE_EXCLUSIVE, Op, OpKind, OpsDoc, REPLACE};''')
rep('''  /// `from`'s tree at `version` copied to `to`.
  fn copy(&mut self, from: &str, to: &str, version: u64) -> &mut Work {
    let from = self.name(from);
    self.push(OpKind::Copy, to, version, 0, from)
  }
''','''  /// `from`'s tree at `version` copied to `to`.
  fn copy(&mut self, from: &str, to: &str, version: u64) -> &mut Work {
    let from = self.name(from);
    self.push(OpKind::Copy, to, version, 0, from)
  }

  /// `from`'s tree at `version` copied to `to`, replacing what `to` holds (A-626).
  fn replace(&mut self, from: &str, to: &str, version: u64) -> &mut Work {
    let from = self.name(from);
    let flags = self.flags;
    self.flags = flags | REPLACE;
    self.push(OpKind::Copy, to, version, 0, from);
    self.flags = flags;
    self
  }

  /// A copy as the oracle declares one.
  fn declared(&mut self, copy: &Declared) -> &mut Work {
    if copy.replace {
      self.replace(&copy.src, &copy.dst, copy.version)
    } else {
      self.copy(&copy.src, &copy.dst, copy.version)
    }
  }
''')
# refusal test: the nested case now accepted; replaces refused
rep('''/// A-586 (the verdict's refusals, each naming the destination by kind). Do: against a green holding a trunk, a hard
/// link under `h`, a file under `taken`, a trunk changed after version 1 and a path removed after version 3, submit
/// each refused copy. Expect: each a conflict with exactly its window, and the green unchanged: its head, its identity
/// and the copies it grafted.''','''/// A-586 and A-626 (the verdict's refusals, each naming the destination by kind). Do: against a green holding a trunk,
/// a hard link under `h`, a file under `taken`, a trunk changed after version 1 and a path removed after version 3,
/// submit each refused copy: among them a second copy onto a place the first copy's tree fills, and replaces of a tree
/// changed after the base, of a tree holding a hard link and of a link's target. Expect: each a conflict with exactly
/// its window, and the green unchanged: its head, its identity and the copies it grafted.''')
rep('''  let mut nested = Work::exclusive();
  nested.copy("t2", "y", 4).copy("t2", "y/z", 4);''','''  let mut nested = Work::exclusive();
  nested.copy("t2", "y", 5).copy("trunk", "y/a", 5);''')
rep('''  use MergeConflictClass::{CreateCreate, RenameRename, TypeChanged};''','''  use MergeConflictClass::{CreateCreate, DeleteModify, RenameRename, TypeChanged};''')
rep('''    (nested, 5, vec![("y/z", TypeChanged)]),
    (moved, 5, vec![("y/q", TypeChanged)]),
  ];''','''    (nested, 5, vec![("y/a", CreateCreate)]),
    (moved, 5, vec![("y/q", TypeChanged)]),
    (
      replace_copy("t2", "taken", 5),
      1,
      vec![("taken", DeleteModify)],
    ),
    (replace_copy("t2", "h", 5), 5, vec![("h", TypeChanged)]),
    (
      replace_copy("t2", "trunk/a", 5),
      5,
      vec![("trunk/a", TypeChanged)],
    ),
  ];''')
# drop the old oracle, keep base_paths
start=s.index("/// The paths the oracle's histories name:")
s=s[:start]
tests=open(scratch+'/a626_tests.rs').read()
oracle=open(scratch+'/a626_oracle.rs').read()
s=s+tests+'\n'+oracle
open(p,'w').write(s)
print("ok", len(s.splitlines()))
