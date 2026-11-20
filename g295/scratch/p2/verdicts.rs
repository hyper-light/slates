  /// Each copy's fix in `r` (A-626 part 2), by destination, computed once a verdict.
  pub(super) fn copy_fixes(&self, r: &Resolved) -> CopyFixes {
    r.copies
      .iter()
      .map(|(dst, spec)| (dst.clone(), self.copy_fix(&spec.source, spec.version)))
      .collect()
  }

  /// The source of the copy `spec` as it brings it, with its fix (`fix`, from [`Green::copy_fixes`]) where it has one.
  fn source_at<'a>(
    &'a self,
    spec: &'a CopySpec,
    fix: Option<&'a Result<Option<CopyFix>, MergeConflictClass>>,
  ) -> SourceAt<'a> {
    SourceAt {
      green: self,
      source: &spec.source,
      fix: fix
        .and_then(|fix| fix.as_ref().ok())
        .and_then(Option::as_ref),
    }
  }

  /// The verdict of each copy the increment declares (module docs), its effect or its conflict: a copy into another's
  /// tree is judged against what that copy brings there (A-626), every other against the head.
  pub(super) fn decide_copies(
    &self,
    inc: &Increment,
    (r, fixes): (&Resolved, &CopyFixes),
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for (dst, spec) in &r.copies {
      match self.decide_copy(inc.base, (dst, spec), (r, fixes)) {
        Ok(effect) => effects.push(effect),
        Err(window) => windows.push(window),
      }
    }
  }

  /// One copy's verdict (module docs); the conflict names the destination, by kind.
  fn decide_copy(
    &self,
    base: u64,
    (dst, spec): (&str, &CopySpec),
    (r, fixes): (&Resolved, &CopyFixes),
  ) -> Result<Effect, ConflictWindow> {
    let fix = match fixes.get(dst) {
      Some(Err(class)) => return Err(Green::window(dst, *class)),
      Some(Ok(fix)) => fix.as_ref(),
      None => None,
    };
    if let Some(class) = self.destination_refused_in(base, (dst, spec), (r, fixes)) {
      return Err(Green::window(dst, class));
    }
    self.copy_of_source(dst, spec, fix)
  }

  /// Why `dst` takes no copy, if it takes none: against the head ([`Green::destination_refused`]), or, inside another
  /// copy of the increment (A-626, a second copy into a copied tree; Subversion's move within one is a copy and a
  /// removal), against what that copy brings there, which its verdict holds as its source's tree at its version: a tree
  /// there takes the copy only as a replace.
  fn destination_refused_in(
    &self,
    base: u64,
    (dst, spec): (&str, &CopySpec),
    (r, fixes): (&Resolved, &CopyFixes),
  ) -> Option<MergeConflictClass> {
    let Some((outer, outer_spec)) = outer_of(&r.copies, dst) else {
      if dst.is_empty() {
        return Some(MergeConflictClass::TypeChanged);
      }
      return self.destination_refused(base, dst, spec.replace);
    };
    let there = joined(&outer_spec.source, relative(dst, outer).unwrap_or_default());
    let brought = self.source_at(outer_spec, fixes.get(outer));
    (brought.holds_tree(&there) && !spec.replace).then_some(MergeConflictClass::CreateCreate)
  }

  /// Why the head's `dst` takes no copy, if it takes none: a tree there and no replace (`CreateCreate`, an add of a
  /// present path), a replaced tree changed after the base (`DeleteModify`, a delete against a change) or holding a hard
  /// link or a link's target (`TypeChanged`: Subversion has none, A-626), or an absent tree changed after the base
  /// (`CreateCreate`).
  fn destination_refused(&self, base: u64, dst: &str, replace: bool) -> Option<MergeConflictClass> {
    if !self.tree_held(dst) {
      return self
        .tree_changed_after(dst, base)
        .then_some(MergeConflictClass::CreateCreate);
    }
    if !replace {
      return Some(MergeConflictClass::CreateCreate);
    }
    if self.tree_changed_after(dst, base) {
      return Some(MergeConflictClass::DeleteModify);
    }
    (self.hardlinks.holds_tree(dst) || self.links_to.holds_tree(dst))
      .then_some(MergeConflictClass::TypeChanged)
  }

  /// The source's half of a copy's verdict (module docs), and the copy's effect: refused when the source version is past
  /// the head, when nothing stood in the source's tree at it, and when the source's tree holds a hard link
  /// (`TypeChanged`). A source changed after its version is copied as it stood then, through its fix (A-626 part 2).
  fn copy_of_source(
    &self,
    dst: &str,
    spec: &CopySpec,
    fix: Option<&CopyFix>,
  ) -> Result<Effect, ConflictWindow> {
    let refused = |class| Err(Green::window(dst, class));
    if spec.version > self.head() {
      return refused(MergeConflictClass::TypeChanged);
    }
    let brought = SourceAt {
      green: self,
      source: &spec.source,
      fix,
    };
    if !brought.holds_tree(&spec.source) || self.hardlinks.holds_tree(&spec.source) {
      return refused(MergeConflictClass::TypeChanged);
    }
    Ok(Effect::CopyTree {
      dst: dst.to_owned(),
      from: spec.source.clone(),
      version: spec.version,
      replace: spec.replace,
      fix: fix.cloned().map(Box::new),
    })
  }

  /// What a copy of `source` at `version` changes in the head's tree it grafts, so it brings the tree as it stood at the
  /// version (A-626 part 2; FSFS copies from any committed revision): none for a tree unchanged since. The paths the
  /// change index names in the source's tree since the version are the only keys that can differ (a key unnamed since
  /// is unchanged); a copy made since above the source replaced the whole tree, so nothing of the head's is grafted; one
  /// made since inside it is cut. Refused, by class, when the version's values are folded away (below the fold floor or
  /// the checkpoint the green was restored at) or a hard link stood in the tree at it. O(the change index's entries
  /// since the version, and the keys at the paths it names).
  pub(super) fn copy_fix(
    &self,
    source: &str,
    version: u64,
  ) -> Result<Option<CopyFix>, MergeConflictClass> {
    if !self.tree_changed_after(source, version) {
      return Ok(None);
    }
    if version < self.folded_below.max(self.deltas_from) {
      return Err(MergeConflictClass::TypeChanged);
    }
    let named: BTreeSet<&str> = self
      .changes
      .range(version.saturating_add(1)..)
      .flat_map(|(_, entries)| entries)
      .map(|entry| entry.path.as_str())
      .filter(|path| in_tree(path, source))
      .collect();
    let start_empty = self.replaced_after(source, version);
    let mut fix = CopyFix {
      start_empty,
      ..CopyFix::default()
    };
    if !start_empty {
      fix.cuts = self.cuts_after(source, version, &named);
    }
    for path in named {
      self.fix_nodes(source, (path, version), &mut fix)?;
      self.fix_attributes(source, (path, version), &mut fix);
    }
    Ok(Some(fix))
  }

  /// The trees of the copies made inside `source` after `version` (each a path the change index names, `named`),
  /// relative to the source, the outermost of each nest only.
  fn cuts_after(&self, source: &str, version: u64, named: &BTreeSet<&str>) -> BTreeSet<String> {
    let mut cuts: BTreeSet<String> = BTreeSet::new();
    for path in named {
      let copied = self
        .copies
        .get(*path)
        .is_some_and(|records| records.iter().any(|record| record.version > version));
      let Some(rel) = relative(path, source).filter(|rel| !rel.is_empty()) else {
        continue;
      };
      if copied && !cuts.iter().any(|cut| in_tree(rel, cut)) {
        cuts.retain(|cut| !in_tree(cut, rel));
        cuts.insert(rel.to_owned());
      }
    }
    cuts
  }

  /// [`Green::copy_fix`] of the nodes at `path` (in `source`'s tree): its file, IPC node, directory, mode and symbolic
  /// link at `version`, each recorded where it differs from what the head's graft brings; a hard link then refuses.
  fn fix_nodes(
    &self,
    source: &str,
    (path, version): (&str, u64),
    fix: &mut CopyFix,
  ) -> Result<(), MergeConflictClass> {
    if self.hardlink_at(path, version).is_some() {
      return Err(MergeConflictClass::TypeChanged);
    }
    let rel = relative(path, source).unwrap_or_default().to_owned();
    let grafted = grafted_in(fix, &rel);
    let then = self.content_ref_at(path, version);
    if then.as_deref() != self.content.get(path).filter(|_| grafted) {
      fix
        .files
        .insert(rel.clone(), then.map(std::borrow::Cow::into_owned));
    }
    let then = self.special_at(path, version);
    if then != self.specials.get(path).copied().filter(|_| grafted) {
      fix.specials.insert(rel.clone(), then);
    }
    let then = self.dir_at(path, version);
    if then != (grafted && self.dirs.contains_key(path)) {
      fix.dirs.insert(rel.clone(), then);
    }
    let then = self.mode_at(path, version);
    if then != self.modes.get(path).copied().filter(|_| grafted) {
      fix.modes.insert(rel.clone(), then);
    }
    let then = self.symlink_at(path, version);
    if then.as_ref() != self.symlinks.get(path).filter(|_| grafted) {
      fix.symlinks.insert(rel, then);
    }
    Ok(())
  }

  /// [`Green::copy_fix`] of the attributes at `path`: every name it carries now or carried, each recorded at `version`
  /// where it differs from what the head's graft brings.
  fn fix_attributes(&self, source: &str, (path, version): (&str, u64), fix: &mut CopyFix) {
    let rel = relative(path, source).unwrap_or_default().to_owned();
    let grafted = grafted_in(fix, &rel);
    let from = (path.to_owned(), String::new());
    let names: BTreeSet<String> = self
      .xattrs
      .of(path)
      .map(|(name, _)| name)
      .chain(
        self
          .xattr_history
          .range(from..)
          .take_while(|((at, _), _)| at == path)
          .map(|((_, name), _)| name.clone()),
      )
      .collect();
    for name in names {
      let then = self.xattr_at(path, &name, version);
      if then.as_ref() != self.xattrs.get(path, &name).filter(|_| grafted) {
        fix.xattrs.insert((rel.clone(), name), then);
      }
    }
  }

  /// The operations on this increment's copied trees (module docs), judged against the copied tree: its copy's source
  /// as the copy brings it.
  pub(super) fn decide_copied(
    &self,
    inc: &Increment,
    (r, fixes): (&Resolved, &CopyFixes),
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    let Some(copied) = r.copied.as_deref() else {
      return;
    };
    let from = |path: &str| self.copied_source((r, fixes), path);
    self.decide_copied_content(inc, copied, &from, effects, windows);
    Green::decide_copied_names(copied, &from, effects, windows);
    Green::decide_copied_metadata(copied, &from, effects, windows);
  }

  /// Where `path`, in a copied tree of `r`, comes from: its innermost copy's source as that copy brings it, and its
  /// place there (`path` itself, outside every copy: never, for the paths the split hands over).
  fn copied_source<'a>(
    &'a self,
    (r, fixes): (&'a Resolved, &'a CopyFixes),
    path: &str,
  ) -> (SourceAt<'a>, String) {
    match innermost(&r.copies, path) {
      Some((dst, spec)) => (
        self.source_at(spec, fixes.get(dst)),
        joined(&spec.source, relative(path, dst).unwrap_or_default()),
      ),
      None => (
        SourceAt {
          green: self,
          source: "",
          fix: None,
        },
        path.to_owned(),
      ),
    }
  }

  /// [`Green::decide_copied`] of creations and edits of files.
  fn decide_copied_content<'a>(
    &'a self,
    inc: &Increment,
    copied: &Resolved,
    from: &impl Fn(&str) -> (SourceAt<'a>, String),
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for path in &copied.creates {
      let ops = copied.content.get(path).map(Vec::as_slice).unwrap_or(&[]);
      let (brought, at) = from(path);
      let verdict = Green::copied_create(path, (&brought, at.as_str()), ops, &inc.post_state);
      record(effects, windows, verdict);
    }
    for (path, ops) in &copied.content {
      if !copied.creates.contains(path) {
        let (brought, at) = from(path);
        let verdict = Green::copied_edit(path, (&brought, at.as_str()), ops, &inc.post_state);
        record(effects, windows, verdict);
      }
    }
    for (path, node) in &copied.specials {
      let (brought, at) = from(path);
      let effect = Effect::Special(path.clone(), *node);
      record(
        effects,
        windows,
        Green::unless(brought.holds_node(&at), path, effect),
      );
    }
  }

  /// [`Green::decide_copied`] of removals and directories.
  fn decide_copied_names<'a>(
    copied: &Resolved,
    from: &impl Fn(&str) -> (SourceAt<'a>, String),
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for path in &copied.unlinks {
      let (brought, at) = from(path);
      record(
        effects,
        windows,
        Ok(Green::copied_unlink(path, (&brought, at.as_str()))),
      );
    }
    for path in &copied.mkdirs {
      let (brought, at) = from(path);
      record(
        effects,
        windows,
        Green::copied_mkdir(path, (&brought, at.as_str())),
      );
    }
    let cleared: BTreeSet<String> = copied
      .cleared()
      .iter()
      .map(|path| from(path.as_str()).1)
      .collect();
    for path in &copied.rmdirs {
      let (brought, at) = from(path);
      record(
        effects,
        windows,
        Green::copied_rmdir(path, (&brought, at.as_str()), &cleared),
      );
    }
  }

  /// [`Green::decide_copied`] of modes, symbolic links and attributes.
  fn decide_copied_metadata<'a>(
    copied: &Resolved,
    from: &impl Fn(&str) -> (SourceAt<'a>, String),
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    let absent = |path: &String| {
      let (brought, at) = from(path);
      !brought.node_stands(&at) && !copied.establishes(path)
    };
    for (path, mode) in &copied.modes {
      let effect = Effect::Mode(path.clone(), *mode);
      record(
        effects,
        windows,
        Green::unless_absent(absent(path), path, effect),
      );
    }
    for (path, target) in &copied.symlinks {
      let (brought, at) = from(path);
      let refused = brought.holds_node(&at) && brought.symlink(&at).is_none();
      let effect = Effect::Symlink(path.clone(), target.clone());
      record(effects, windows, Green::unless(refused, path, effect));
    }
    for (path, name, value) in &copied.xattr_sets {
      let effect = Effect::SetXattr(path.clone(), name.clone(), value.clone());
      record(
        effects,
        windows,
        Green::unless_absent(absent(path), path, effect),
      );
    }
    for (path, name) in &copied.xattr_removes {
      let (brought, at) = from(path);
      let held = brought.xattr(&at, name).is_some();
      let effect = held.then(|| Effect::RemoveXattr(path.clone(), name.clone()));
      record(effects, windows, Ok(effect));
    }
  }

