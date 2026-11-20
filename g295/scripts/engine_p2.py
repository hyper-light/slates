import sys
p=sys.argv[1]
s=open(p).read()
def rep(old,new,count=1):
    global s
    n=s.count(old)
    if n!=count:
        raise SystemExit(f"anchor count {n} != {count}: {old[:80]!r}")
    s=s.replace(old,new)
rep('''  /// Copy a tree by reference (A-586): its destination, its source, the source's version, and whether it replaces what
  /// the destination holds (A-626).
  CopyTree(String, String, u64, bool),''','''  /// Copy a tree by reference (A-586): its destination, its source, the source's version, whether it replaces what the
  /// destination holds (A-626), and what it changes in the head's tree it grafts when the source changed after the
  /// version (A-626 part 2).
  CopyTree {
    dst: String,
    from: String,
    version: u64,
    replace: bool,
    fix: Option<Box<crate::chain_entry::CopyFix>>,
  },''')
rep('''      Effect::CopyTree(dst, from, version, replace) => Applied::CopyTree {
        dst: dst.clone(),
        from: from.clone(),
        version: *version,
        replace: *replace,
      },''','''      Effect::CopyTree {
        dst,
        from,
        version,
        replace,
        fix,
      } => Applied::CopyTree {
        dst: dst.clone(),
        from: from.clone(),
        version: *version,
        replace: *replace,
        fix: fix.clone(),
      },''')
rep('''      | Effect::CopyTree(path, ..) => path,''','''      | Effect::CopyTree { dst: path, .. } => path,''')
rep('''      Effect::CopyTree(dst, ..) => IdentityKey::Copy(dst.clone()),''','''      Effect::CopyTree { dst, .. } => IdentityKey::Copy(dst.clone()),''')
rep('''Effect::CopyTree(..) => {}''','''Effect::CopyTree { .. } => {}''', count=2)
rep('''      Applied::CopyTree {
        dst,
        from,
        version,
        replace,
      } => Effect::CopyTree(dst.clone(), from.clone(), *version, *replace),''','''      Applied::CopyTree {
        dst,
        from,
        version,
        replace,
        fix,
      } => Effect::CopyTree {
        dst: dst.clone(),
        from: from.clone(),
        version: *version,
        replace: *replace,
        fix: fix.clone(),
      },''')
rep('''  fn effect_of(&self, applied: &crate::chain_entry::Applied, post_state: &[u8]) -> Effect {''','''  fn effect_of(
    &self,
    applied: &crate::chain_entry::Applied,
    (post_state, copies): (&[u8], &copy::EntryCopies<'_>),
  ) -> Effect {''')
rep('''      Applied::Copied { dst, from, ops } => {
        let source = self.content.get(from).unwrap_or_default();''','''      Applied::Copied { dst, from, ops } => {
        // A file a copy brings and the same version edits reads the bytes the copy brings (A-626 part 2).
        let source = copies.source_at(self, dst).content(from).unwrap_or_default();''')
rep('''    let effects = applied
      .iter()
      .map(|effect| self.effect_of(effect, &inc.post_state))
      .collect();''','''    let copies = copy::EntryCopies::of(applied);
    let effects = applied
      .iter()
      .map(|effect| self.effect_of(effect, (&inc.post_state, &copies)))
      .collect();''')
rep('''    let mut effects = Vec::new();
    let mut windows: Vec<_> = r
      .invalid
      .iter()
      .chain(self.unfit_content(inc, r).iter())
      .chain(self.unfit_copied(inc, r).iter())''','''    let mut effects = Vec::new();
    // Each copy's fix once (A-626 part 2): its copied tree is judged against it, and its effect carries it.
    let fixes = self.copy_fixes(r);
    let mut windows: Vec<_> = r
      .invalid
      .iter()
      .chain(self.unfit_content(inc, r).iter())
      .chain(self.unfit_copied(inc, (r, &fixes)).iter())''')
rep('''    self.decide_copies(inc, r, &mut effects, &mut windows);
    self.decide_copied(inc, r, &mut effects, &mut windows);''','''    self.decide_copies(inc, (r, &fixes), &mut effects, &mut windows);
    self.decide_copied(inc, (r, &fixes), &mut effects, &mut windows);''')
open(p,'w').write(s)
print("ok")
