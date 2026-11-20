  /// A file created at `path` in a copied tree, where the copy brings nothing (`from`, its place in the source as the
  /// copy brings it).
  fn copied_create(
    path: &str,
    (brought, from): (&SourceAt<'_>, &str),
    ops: &[Op],
    post_state: &[u8],
  ) -> Result<Option<Effect>, ConflictWindow> {
    if brought.holds_node(from) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    let bytes = apply(&[], ops, post_state);
    let whole = vec![insert_whole(bytes.len() as u64)];
    Ok(Some(Effect::SetContent(
      path.to_owned(),
      bytes,
      whole,
      ContentSource::Created(ops.to_vec()),
    )))
  }

  /// A file the copy brings to `path` (from `from`), edited: the bytes the copy brings with the edits spliced in,
  /// recorded as a rename's edited destination is (A-547), so replay splices them into the same bytes as the verdict did
  /// (a fixed copy's, A-626 part 2, read from its entry: `EntryCopies`).
  fn copied_edit(
    path: &str,
    (brought, from): (&SourceAt<'_>, &str),
    ops: &[Op],
    post_state: &[u8],
  ) -> Result<Option<Effect>, ConflictWindow> {
    let Some(source) = brought.content(from) else {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    };
    let bytes = apply(source, ops, post_state);
    let whole = vec![insert_whole(bytes.len() as u64)];
    Ok(Some(Effect::SetContent(
      path.to_owned(),
      bytes,
      whole,
      ContentSource::Copied(from.to_owned(), ops.to_vec()),
    )))
  }

  /// A name the copy brings to `path` removed: whatever its source (`from`) is, a file, a symbolic link or an IPC node;
  /// nothing for a name the copy does not bring, as an unlink of an absent name is nothing (`merge_unlink`).
  fn copied_unlink(path: &str, (brought, from): (&SourceAt<'_>, &str)) -> Option<Effect> {
    if brought.content(from).is_some() {
      Some(Effect::RemoveContent(path.to_owned()))
    } else if brought.symlink(from).is_some() {
      Some(Effect::RemoveSymlink(path.to_owned()))
    } else if brought.special(from).is_some() {
      Some(Effect::RemoveSpecial(path.to_owned()))
    } else {
      None
    }
  }

  /// A directory made at `path` in a copied tree: nothing where the copy brings one, a type conflict where it brings
  /// another kind.
  fn copied_mkdir(
    path: &str,
    (brought, from): (&SourceAt<'_>, &str),
  ) -> Result<Option<Effect>, ConflictWindow> {
    if brought.dir(from) {
      return Ok(None);
    }
    if brought.holds_node(from) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    Ok(Some(Effect::MakeDir(path.to_owned())))
  }

  /// A directory the copy brings to `path` removed, once the increment's own removals (`cleared`, in the source's
  /// paths) empty it, as `merge_rmdir` judges one.
  fn copied_rmdir(
    path: &str,
    (brought, from): (&SourceAt<'_>, &str),
    cleared: &BTreeSet<String>,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if !brought.dir(from) {
      if brought.holds_node(from) {
        return Err(Green::window(path, MergeConflictClass::TypeChanged));
      }
      return Ok(None);
    }
    // The directory's own key is no child of it (the root's tree holds the root's own key).
    let mut gone = cleared.clone();
    gone.insert(from.to_owned());
    if brought.has_live_child(&beneath_prefix(from), &gone) {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    Ok(Some(Effect::RemoveDir(path.to_owned())))
  }

  /// The paths whose content ops in this increment's copied trees do not fit what they edit (`super::fits_base`): a
  /// created path edits nothing, an edited one the bytes its copy brings.
  pub(super) fn unfit_copied(&self, inc: &Increment, (r, fixes): (&Resolved, &CopyFixes)) -> Vec<String> {
    let Some(copied) = r.copied.as_deref() else {
      return Vec::new();
    };
    let post_len = u64::try_from(inc.post_state.len()).unwrap_or(u64::MAX);
    copied
      .content
      .iter()
      .filter(|(path, ops)| {
        let base_len = if copied.creates.contains(*path) {
          0
        } else {
          let (brought, from) = self.copied_source((r, fixes), path);
          brought
            .content(&from)
            .map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        };
        !super::fits_base(ops, base_len, post_len)
      })
      .map(|(path, _)| path.clone())
      .collect()
  }

  /// How many keys `path`'s tree holds in every map (A-626 part 2): descents and the counts their nodes keep, no walk.
  pub(super) fn count_tree(&self, path: &str) -> usize {
    [
      self.content.count_tree(path),
      self.specials.count_tree(path),
      self.dirs.count_tree(path),
      self.modes.count_tree(path),
      self.symlinks.count_tree(path),
      self.hardlinks.count_tree(path),
      self.xattrs.count_tree(path),
    ]
    .into_iter()
    .fold(0, usize::saturating_add)
  }

  /// The room a chain entry for `inc` needs beyond what its increment declares (A-626 part 2): the fix each copy from a
  /// version its source changed after records, as `chain_entry::fix_bytes` counts it. Derived from what this green's
  /// histories hold now, never a fixed allowance: the service adds it to `chain_entry::entry_bound` before the verdict.
  /// Nothing for an increment without such a copy.
  pub fn copy_entry_room(&self, inc: &Increment) -> usize {
    let r = Green::resolve(inc);
    self
      .copy_fixes(&r)
      .values()
      .filter_map(|fix| fix.as_ref().ok().and_then(Option::as_ref))
      .map(crate::chain_entry::fix_bytes)
      .fold(0, usize::saturating_add)
  }

