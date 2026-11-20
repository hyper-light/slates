  /// Applies the copies among a version's accepted effects, in their destinations' order, an outer copy before one
  /// inside its tree (module docs): every source held first, as the version before this one, whatever a copy of this
  /// version takes away (FSFS copies from a revision, never a transaction); then each destination's tree taken away where
  /// the copy replaces it, the held source grafted there, and a fixed copy's fix applied (A-626 part 2). The paths a
  /// replace took away, for the change index.
  pub(super) fn commit_copies(&mut self, effects: &[Effect], version: u64) -> BTreeSet<String> {
    let mut copies: Vec<Committed<'_>> = effects
      .iter()
      .filter_map(|effect| match effect {
        Effect::CopyTree {
          dst,
          from,
          version,
          replace,
          fix,
        } => Some((
          (dst.as_str(), from.as_str()),
          (*version, *replace),
          fix.as_deref(),
        )),
        _ => None,
      })
      .collect();
    copies.sort_by(|a, b| a.0.0.cmp(b.0.0));
    // A source whose copy starts empty brings nothing of the head's, so it is not held.
    let held: Vec<Option<HeldSource>> = copies
      .iter()
      .map(|((_, src), _, fix)| {
        let grafted = fix.is_none_or(|fix| !fix.start_empty);
        grafted.then(|| self.hold_source(src))
      })
      .collect();
    let mut removed = BTreeSet::new();
    for (((dst, src), (source_version, replace), fix), held) in copies.into_iter().zip(held) {
      if replace {
        removed.extend(self.clear_tree(dst, version));
      }
      self.graft_source(held, (dst, src, source_version), version);
      if let Some(fix) = fix {
        self.apply_fix(dst, fix);
      }
    }
    removed
  }

  /// Every map's hold on `src`'s tree ([`HeldSource`]).
  fn hold_source(&mut self, src: &str) -> HeldSource {
    HeldSource {
      content: self.content.hold_tree(src),
      specials: self.specials.hold_tree(src),
      dirs: self.dirs.hold_tree(src),
      modes: self.modes.hold_tree(src),
      symlinks: self.symlinks.hold_tree(src),
      hardlinks: self.hardlinks.hold_tree(src),
      xattrs: self.xattrs.hold_tree(src),
    }
  }

  /// Grafts a held source at its copy's destination, at `version` (module docs): every map's graft (the reverse map of
  /// hard links has none: the source holds no link), and the copy's one record, `last_changed` entry and change-index
  /// bytes; nothing grafted for a copy that starts empty (`None`). A map that refuses what the verdict judged possible is
  /// counted ([`Green::copies_refused`]), never silently.
  fn graft_source(
    &mut self,
    held: Option<HeldSource>,
    (dst, src, source_version): (&str, &str, u64),
    version: u64,
  ) {
    if let Some(held) = held {
      let results = [
        self.content.graft_tree(held.content, dst),
        self.specials.graft_tree(held.specials, dst),
        self.dirs.graft_tree(held.dirs, dst),
        self.modes.graft_tree(held.modes, dst),
        self.symlinks.graft_tree(held.symlinks, dst),
        self.hardlinks.graft_tree(held.hardlinks, dst),
        self.xattrs.graft_tree(held.xattrs, dst),
      ];
      if results.iter().any(Result::is_err) {
        self.copies_refused = self.copies_refused.saturating_add(1);
      }
      if results.iter().flatten().any(|copied| copied.brought()) {
        self.copies_grafted = self.copies_grafted.saturating_add(1);
      }
    }
    self.last_changed.insert(dst.to_owned(), version);
    self.changes_bytes = self
      .changes_bytes
      .saturating_add(appearance_bytes(dst, src));
    self
      .copies
      .entry(dst.to_owned())
      .or_default()
      .push(Appearance {
        version,
        source: src.to_owned(),
        source_version,
      });
  }

  /// Brings what `dst` holds from the head's source tree to the source's tree at the copied version (A-626 part 2): the
  /// fix's cuts taken away, then each of its values set. None of it is recorded key by key: the copy's record stands for
  /// the tree it brings, its identity entry with it, and a key's first change after the copy records the value it gave.
  fn apply_fix(&mut self, dst: &str, fix: &CopyFix) {
    for cut in &fix.cuts {
      let path = joined(dst, cut);
      self.content.cut_tree(&path);
      self.specials.cut_tree(&path);
      self.dirs.cut_tree(&path);
      self.modes.cut_tree(&path);
      self.symlinks.cut_tree(&path);
      self.hardlinks.cut_tree(&path);
      self.xattrs.cut_tree(&path);
    }
    for (rel, bytes) in &fix.files {
      let path = joined(dst, rel);
      if let Some(bytes) = bytes {
        self.content.insert(&path, bytes.clone());
      } else {
        self.content.remove(&path);
      }
    }
    for (rel, node) in &fix.specials {
      let path = joined(dst, rel);
      if let Some(node) = node {
        self.specials.insert(&path, *node);
      } else {
        self.specials.remove(&path);
      }
    }
    for (rel, stood) in &fix.dirs {
      let path = joined(dst, rel);
      if *stood {
        self.dirs.insert(&path, ());
      } else {
        self.dirs.remove(&path);
      }
    }
    self.apply_fix_metadata(dst, fix);
  }

  /// [`Green::apply_fix`]'s modes, symbolic links and attributes.
  fn apply_fix_metadata(&mut self, dst: &str, fix: &CopyFix) {
    for (rel, mode) in &fix.modes {
      let path = joined(dst, rel);
      if let Some(mode) = mode {
        self.modes.insert(&path, *mode);
      } else {
        self.modes.remove(&path);
      }
    }
    for (rel, target) in &fix.symlinks {
      let path = joined(dst, rel);
      if let Some(target) = target {
        self.symlinks.insert(&path, target.clone());
      } else {
        self.symlinks.remove(&path);
      }
    }
    for ((rel, name), value) in &fix.xattrs {
      let path = joined(dst, rel);
      if let Some(value) = value {
        self.xattrs.insert(&path, name, value.clone());
      } else {
        self.xattrs.remove(&path, name);
      }
    }
  }

