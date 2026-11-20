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
edit('crates/merge/src/ptrie.rs',[
('''  /// Whether a key starts with `prefix`: one descent, no walk and no count taken.
  pub fn has_prefix(&self, nodes: &Nodes<V>, prefix: &[u8]) -> bool {''','''  /// How many keys start with `prefix`: one descent and the count its top node keeps, no walk (A-626).
  pub fn prefix_len(&self, nodes: &Nodes<V>, prefix: &[u8]) -> usize {
    self
      .under(nodes, prefix)
      .map_or(0, |(slot, _, _)| nodes.count(slot))
  }

  /// Whether a key starts with `prefix`: one descent, no walk and no count taken.
  pub fn has_prefix(&self, nodes: &Nodes<V>, prefix: &[u8]) -> bool {'''),
])
edit('crates/merge/src/dims/copy.rs',[
('''  /// [`Dim::holds_tree`] over the path's bytes.
  fn holds_tree_of(&self, path: &[u8]) -> bool {''','''  /// How many keys `path`'s tree holds: its own and those beneath it (every key, for the root). Descents and the counts
  /// their nodes keep, no walk (A-626: whether a copy's source held anything at the version it is copied at).
  pub(crate) fn count_tree(&self, path: &str) -> usize {
    if path.is_empty() {
      return self.root.len(&self.nodes);
    }
    let own = usize::from(self.root.contains_key(&self.nodes, path.as_bytes()));
    own.saturating_add(
      self
        .root
        .prefix_len(&self.nodes, &below(path.as_bytes())),
    )
  }

  /// [`Dim::holds_tree`] over the path's bytes.
  fn holds_tree_of(&self, path: &[u8]) -> bool {'''),
('''  /// Whether `path` or a path beneath it carries an attribute (any path, for the root). Descents, no walk.
  pub(crate) fn holds_tree(&self, path: &str) -> bool {''','''  /// How many attributes `path` and the paths beneath it carry (every one, for the root): descents and counts, no walk.
  pub(crate) fn count_tree(&self, path: &str) -> usize {
    if path.is_empty() {
      return self.map.root.len(&self.map.nodes);
    }
    [path_key(path), below(&escaped(path))]
      .iter()
      .map(|prefix| self.map.root.prefix_len(&self.map.nodes, prefix))
      .fold(0, usize::saturating_add)
  }

  /// Whether `path` or a path beneath it carries an attribute (any path, for the root). Descents, no walk.
  pub(crate) fn holds_tree(&self, path: &str) -> bool {'''),
])
print("ok")
