import sys
p=sys.argv[1]
s=open(p).read()
def rep(old,new,count=1):
    global s
    n=s.count(old)
    if n!=count:
        raise SystemExit(f"anchor count {n} != {count}: {old[:80]!r}")
    s=s.replace(old,new)
# refusal test: a copy from a changed source is no longer refused
rep('''    (one_copy("trunk", "y", 4), 5, vec![("y", RenameRename)]),
''','')
rep('''  use MergeConflictClass::{CreateCreate, DeleteModify, RenameRename, TypeChanged};''','''  use MergeConflictClass::{CreateCreate, DeleteModify, TypeChanged};''')
# model: sources at their versions
rep('''  fn copies_stand(&self, copies: &[Declared]) -> bool {
    let head = self.current();
    copies.iter().all(|copy| {
      let place = outer_copy(copies, &copy.dst).map_or_else(
        || copy.dst.clone(),
        |outer| {
          joined(
            &outer.src,
            relative(&copy.dst, &outer.dst).unwrap_or_default(),
          )
        },
      );
      let destination = !copy.dst.is_empty() && (copy.replace || !head.holds_tree(&place));
      let source = head.holds_tree(&copy.src) && !self.changed_after(&copy.src, copy.version);
      destination && source
    })
  }''','''  fn copies_stand(&self, copies: &[Declared]) -> bool {
    let head = self.current();
    let held_at = |version: u64, root: &str| {
      self
        .state(version)
        .is_some_and(|state| state.holds_tree(root))
    };
    copies.iter().all(|copy| {
      let filled = match outer_copy(copies, &copy.dst) {
        None => head.holds_tree(&copy.dst),
        Some(outer) => {
          let rel = relative(&copy.dst, &outer.dst).unwrap_or_default();
          held_at(outer.version, &joined(&outer.src, rel))
        }
      };
      let destination = !copy.dst.is_empty() && (copy.replace || !filled);
      destination && held_at(copy.version, &copy.src)
    })
  }''')
rep('''    let sources: Vec<State> = order
      .iter()
      .map(|copy| next.tree_at(&copy.src, &copy.dst))
      .collect();''','''    let sources: Vec<State> = order
      .iter()
      .map(|copy| {
        self
          .state(copy.version)
          .map(|state| state.tree_at(&copy.src, &copy.dst))
          .unwrap_or_default()
      })
      .collect();''')
# census: copies accepted from a changed source
rep('''  changed_after: usize,''','''  /// Copies accepted from a version their source changed after (A-626 part 2).
  from_changed: usize,''')
rep('''    if !accepted {
      self.refused += 1;
      self.changed_after += usize::from(changed);
      return;
    }''','''    if !accepted {
      self.refused += 1;
      return;
    }
    self.from_changed += usize::from(changed);''')
rep('''    census.changed_after,''','''    census.from_changed,''')
open(p,'w').write(s)
print("ok")
