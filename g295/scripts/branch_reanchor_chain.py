import sys,os
root=sys.argv[1]
def edit(rel, pairs):
    p=os.path.join(root,rel)
    s=open(p).read()
    for old,new in pairs:
        n=s.count(old)
        if n!=1:
            raise SystemExit(f"{rel}: anchor count {n}: {old[:80]!r}")
        s=s.replace(old,new)
    open(p,'w').write(s)
edit('crates/merge/src/engine/branch.rs',[
('''  history_at, last_version, link_entry, mode_entry, present_at, relink, special_entry, xattr_entry,''',
 '''  history_at, last_entry, link_entry, mode_entry, present_at, relink, special_entry, xattr_entry,'''),
('''/// The value a branch sets a key back to, when it differs from the head's:''',
 '''/// One dimension's last change of its own at a path, with the key it names there ([`Green::keys_changed_after`]): the
/// change's version and whether it removed the key.
type LastChange = (Option<(u64, bool)>, fn(String) -> IdentityKey);

/// The value a branch sets a key back to, when it differs from the head's:'''),
('''    let after = |last: Option<u64>| last.is_some_and(|last| last > version);
    let lasts: [(Option<u64>, fn(String) -> IdentityKey); 6] = [
      (
        last_version(self.content_history.get(path)),
        IdentityKey::File,
      ),
      (
        last_version(self.special_history.get(path)),
        IdentityKey::Special,
      ),
      (last_version(self.dir_history.get(path)), IdentityKey::Dir),
      (last_version(self.mode_history.get(path)), IdentityKey::Mode),
      (
        last_version(self.symlink_history.get(path)),
        IdentityKey::Symlink,
      ),
      (
        last_version(self.hardlink_history.get(path)),
        IdentityKey::Hardlink,
      ),
    ];
    let mut keys: Vec<IdentityKey> = lasts
      .into_iter()
      .filter(|(last, _)| after(*last))
      .map(|(_, key)| key(path.to_owned()))
      .collect();''',
 '''    let after = |last: Option<(u64, bool)>| last.is_some_and(|(last, _)| last > version);
    let lasts: &[LastChange] = &[
      (last_entry(self.content_history.get(path)), IdentityKey::File),
      (
        last_entry(self.special_history.get(path)),
        IdentityKey::Special,
      ),
      (last_entry(self.dir_history.get(path)), IdentityKey::Dir),
      (last_entry(self.mode_history.get(path)), IdentityKey::Mode),
      (
        last_entry(self.symlink_history.get(path)),
        IdentityKey::Symlink,
      ),
      (
        last_entry(self.hardlink_history.get(path)),
        IdentityKey::Hardlink,
      ),
    ];
    let mut keys: Vec<IdentityKey> = lasts
      .iter()
      .filter(|(last, _)| after(*last))
      .map(|(_, key)| key(path.to_owned()))
      .collect();'''),
('''        .filter(|(_, history)| after(history.last().map(|(recorded, _)| *recorded)))''',
 '''        .filter(|(_, history)| after(last_entry(Some(*history))))'''),
])
edit('crates/merge/src/engine/reanchor.rs',[
('''        .covered_since(path, super::last_version(Some(*history)))''',
 '''        .covered_since(path, super::last_entry(Some(*history)))'''),
])
edit('crates/merge/src/chain_entry.rs',[
('''  /// Copy `from`'s tree to `dst` by reference (A-586): what the head holds of it, which the verdict found unchanged
  /// since `version`, the source's version the increment declared. Encoded as its two paths; `version` is read back
  /// from the increment's own `Copy` operation, so the entry costs no more than its bound's two strings.
  CopyTree {
    /// The destination.
    dst: String,
    /// The source.
    from: String,
    /// The version of the source the increment copies.
    version: u64,
  },''',
 '''  /// Copy `from`'s tree to `dst` by reference (A-586): what the head holds of it, which the verdict found unchanged
  /// since `version`, the source's version the increment declared, replacing what `dst` holds when `replace` (A-626).
  /// Encoded as its two paths; `version` and `replace` are read back from the increment's own `Copy` operation, so the
  /// entry costs no more than its bound's two strings.
  CopyTree {
    /// The destination.
    dst: String,
    /// The source.
    from: String,
    /// The version of the source the increment copies.
    version: u64,
    /// Whether the copy replaces what the destination holds ([`crate::ops_doc::REPLACE`]).
    replace: bool,
  },'''),
('''      let version = declared_copy(increment, &dst, &from).ok_or(DocDecodeError::BadKind)?;
      Applied::CopyTree { dst, from, version }''',
 '''      let (version, replace) =
        declared_copy(increment, &dst, &from).ok_or(DocDecodeError::BadKind)?;
      Applied::CopyTree {
        dst,
        from,
        version,
        replace,
      }'''),
('''/// The source version of the `Copy` operation `increment` declares from `from` to `dst` (A-586); `None` when it declares
/// none, which no entry the engine recorded holds.
fn declared_copy(increment: &Increment, dst: &str, from: &str) -> Option<u64> {''',
 '''/// The source version of the `Copy` operation `increment` declares from `from` to `dst` (A-586), and whether it replaces
/// (A-626); `None` when it declares none, which no entry the engine recorded holds.
fn declared_copy(increment: &Increment, dst: &str, from: &str) -> Option<(u64, bool)> {'''),
('''      op.kind == OpKind::Copy && path(u64::from(op.path)) == Some(dst) && path(op.src) == Some(from)
    })
    .map(|op| op.at)''',
 '''      op.kind == OpKind::Copy && path(u64::from(op.path)) == Some(dst) && path(op.src) == Some(from)
    })
    .map(|op| (op.at, op.replaces()))'''),
])
print("ok")
