
/// Each copy's fix in one increment (A-626 part 2), by destination, computed once a verdict ([`Green::copy_fixes`]):
/// none for a source unchanged since its version, the class that refuses one whose version's values are folded away or
/// held a hard link.
pub(super) type CopyFixes = BTreeMap<String, Result<Option<CopyFix>, MergeConflictClass>>;

/// One copy as [`Green::commit_copies`] applies it: its destination and source, the source's version and whether it
/// replaces, and its fix.
type Committed<'a> = ((&'a str, &'a str), (u64, bool), Option<&'a CopyFix>);

/// Whether a copy with `fix` grafts the head's keys at `rel` (relative to its source): not where it starts empty, nor
/// in a cut.
fn grafted_in(fix: &CopyFix, rel: &str) -> bool {
  !fix.start_empty && !fix.cuts.iter().any(|cut| in_tree(rel, cut))
}

/// Counts one key of a fix against the head's graft ([`SourceAt::holds_tree`]): one gained where the fix sets a key the
/// graft does not bring, one lost where it takes away one the graft brings.
fn tally(counts: &mut (usize, usize), then: bool, brought: bool) {
  if then && !brought {
    counts.0 = counts.0.saturating_add(1);
  } else if !then && brought {
    counts.1 = counts.1.saturating_add(1);
  }
}

/// A copy's source as the copy brings it (A-626 part 2): the head's tree, or, for a source changed after the copied
/// version, the head's tree less its fix's cuts with its fix's values set, which is the source's tree at the version.
/// What a verdict judges a copied tree against, and what replay reads an edited file's bytes from.
#[derive(Clone, Copy)]
pub(super) struct SourceAt<'a> {
  green: &'a Green,
  source: &'a str,
  fix: Option<&'a CopyFix>,
}

impl<'a> SourceAt<'a> {
  /// Where `path` lies in the source's tree, relative to it, and whether the head's keys there are grafted; `None`
  /// outside the tree.
  fn place<'p>(&self, path: &'p str) -> Option<(&'p str, bool)> {
    let rel = relative(path, self.source)?;
    Some((rel, self.fix.is_none_or(|fix| grafted_in(fix, rel))))
  }

  /// The file's bytes at `path`.
  pub(super) fn content(&self, path: &str) -> Option<&'a [u8]> {
    let Some(fix) = self.fix else {
      return self.green.content.get(path);
    };
    let (rel, grafted) = self.place(path)?;
    match fix.files.get(rel) {
      Some(bytes) => bytes.as_deref(),
      None => self.green.content.get(path).filter(|_| grafted),
    }
  }

  /// Whether a directory stands at `path`.
  fn dir(&self, path: &str) -> bool {
    let Some(fix) = self.fix else {
      return self.green.dirs.contains_key(path);
    };
    let Some((rel, grafted)) = self.place(path) else {
      return false;
    };
    match fix.dirs.get(rel) {
      Some(stood) => *stood,
      None => grafted && self.green.dirs.contains_key(path),
    }
  }

  /// The IPC node at `path`.
  fn special(&self, path: &str) -> Option<SpecialNode> {
    let Some(fix) = self.fix else {
      return self.green.specials.get(path).copied();
    };
    let (rel, grafted) = self.place(path)?;
    match fix.specials.get(rel) {
      Some(node) => *node,
      None => self.green.specials.get(path).copied().filter(|_| grafted),
    }
  }

  /// The symbolic link's target at `path`.
  fn symlink(&self, path: &str) -> Option<&'a str> {
    let head = self.green.symlinks.get(path).map(String::as_str);
    let Some(fix) = self.fix else {
      return head;
    };
    let (rel, grafted) = self.place(path)?;
    match fix.symlinks.get(rel) {
      Some(target) => target.as_deref(),
      None => head.filter(|_| grafted),
    }
  }

  /// Whether a hard link stands at `path` (a fix never sets one: its version held none).
  fn hardlink(&self, path: &str) -> bool {
    self.place(path).is_some_and(|(_, grafted)| grafted) && self.green.hardlinks.contains_key(path)
  }

  /// The attribute `name`'s value at `path`.
  fn xattr(&self, path: &str, name: &str) -> Option<&'a [u8]> {
    let head = self.green.xattrs.get(path, name).map(Vec::as_slice);
    let Some(fix) = self.fix else {
      return head;
    };
    let (rel, grafted) = self.place(path)?;
    match fix.xattrs.get(&(rel.to_owned(), name.to_owned())) {
      Some(value) => value.as_deref(),
      None => head.filter(|_| grafted),
    }
  }

  /// Whether a file, directory or IPC node stands at `path`: what a mode or an attribute can be set on.
  fn node_stands(&self, path: &str) -> bool {
    self.content(path).is_some() || self.dir(path) || self.special(path).is_some()
  }

  /// Whether a file, directory, symbolic link, hard link or IPC node stands at `path`.
  fn holds_node(&self, path: &str) -> bool {
    self.node_stands(path) || self.symlink(path).is_some() || self.hardlink(path)
  }

  /// Whether a file, directory, symbolic link, hard link or IPC node stands beneath `prefix` and is not in `gone`, as
  /// `merge_rmdir`'s check reads the head: the head's names there and the fix's, each judged as the copy brings it.
  fn has_live_child(&self, prefix: &str, gone: &BTreeSet<String>) -> bool {
    let green = self.green;
    let head = green
      .content
      .prefix(prefix)
      .map(|(path, _)| path)
      .chain(green.dirs.prefix(prefix).map(|(path, _)| path))
      .chain(green.symlinks.prefix(prefix).map(|(path, _)| path))
      .chain(green.hardlinks.prefix(prefix).map(|(path, _)| path))
      .chain(green.specials.prefix(prefix).map(|(path, _)| path));
    let fixed = self
      .fix
      .into_iter()
      .flat_map(|fix| {
        fix
          .files
          .keys()
          .chain(fix.dirs.keys())
          .chain(fix.symlinks.keys())
          .chain(fix.specials.keys())
      })
      .map(|rel| joined(self.source, rel))
      .filter(|path| path.starts_with(prefix));
    head
      .chain(fixed)
      .any(|path| !gone.contains(&path) && self.holds_node(&path))
  }

  /// Whether a key of `root`'s tree (in the source's) stands as the copy brings it: the head's keys the graft brings
  /// there, counted, less those its cuts take, then each key of its fix gained or lost against them. Descents and
  /// counts, never a walk of the tree.
  pub(super) fn holds_tree(&self, root: &str) -> bool {
    let Some(fix) = self.fix else {
      return self.green.tree_held(root);
    };
    let Some((rel_root, grafted)) = self.place(root) else {
      return false;
    };
    let brought = if grafted {
      let cut = fix
        .cuts
        .iter()
        .filter(|cut| in_tree(cut, rel_root))
        .map(|cut| self.green.count_tree(&joined(self.source, cut)))
        .fold(0, usize::saturating_add);
      self.green.count_tree(root).saturating_sub(cut)
    } else {
      0
    };
    let mut counts = (0usize, 0usize);
    self.balance_nodes(fix, rel_root, &mut counts);
    self.balance_metadata(fix, rel_root, &mut counts);
    brought.saturating_add(counts.0) > counts.1
  }

  /// [`SourceAt::holds_tree`]'s tally of the fix's files, IPC nodes and directories in `rel_root`'s tree.
  fn balance_nodes(&self, fix: &CopyFix, rel_root: &str, counts: &mut (usize, usize)) {
    let green = self.green;
    let brought = |rel: &str, held: &dyn Fn(&str) -> bool| {
      grafted_in(fix, rel) && held(&joined(self.source, rel))
    };
    for (rel, bytes) in fix.files.iter().filter(|(rel, _)| in_tree(rel, rel_root)) {
      let held = brought(rel, &|path| green.content.contains_key(path));
      tally(counts, bytes.is_some(), held);
    }
    for (rel, node) in fix.specials.iter().filter(|(rel, _)| in_tree(rel, rel_root)) {
      let held = brought(rel, &|path| green.specials.contains_key(path));
      tally(counts, node.is_some(), held);
    }
    for (rel, stood) in fix.dirs.iter().filter(|(rel, _)| in_tree(rel, rel_root)) {
      let held = brought(rel, &|path| green.dirs.contains_key(path));
      tally(counts, *stood, held);
    }
  }

  /// [`SourceAt::holds_tree`]'s tally of the fix's modes, symbolic links and attributes in `rel_root`'s tree.
  fn balance_metadata(&self, fix: &CopyFix, rel_root: &str, counts: &mut (usize, usize)) {
    let green = self.green;
    let brought = |rel: &str, held: &dyn Fn(&str) -> bool| {
      grafted_in(fix, rel) && held(&joined(self.source, rel))
    };
    for (rel, mode) in fix.modes.iter().filter(|(rel, _)| in_tree(rel, rel_root)) {
      let held = brought(rel, &|path| green.modes.contains_key(path));
      tally(counts, mode.is_some(), held);
    }
    for (rel, target) in fix.symlinks.iter().filter(|(rel, _)| in_tree(rel, rel_root)) {
      let held = brought(rel, &|path| green.symlinks.contains_key(path));
      tally(counts, target.is_some(), held);
    }
    for ((rel, name), value) in fix.xattrs.iter().filter(|((rel, _), _)| in_tree(rel, rel_root)) {
      let held = brought(rel, &|path| green.xattrs.contains_key(path, name));
      tally(counts, value.is_some(), held);
    }
  }
}

/// The copies one chain entry records, for its other effects to read the copied trees by at replay (A-626 part 2): an
/// edited file's bytes before the edit are those its copy brings, a fixed copy's from its fix.
pub(super) struct EntryCopies<'a> {
  copies: Vec<(&'a str, &'a str, Option<&'a CopyFix>)>,
}

impl<'a> EntryCopies<'a> {
  /// The copies `applied` records.
  pub(super) fn of(applied: &'a [Applied]) -> EntryCopies<'a> {
    let copies = applied
      .iter()
      .filter_map(|applied| match applied {
        Applied::CopyTree { dst, from, fix, .. } => {
          Some((dst.as_str(), from.as_str(), fix.as_deref()))
        }
        _ => None,
      })
      .collect();
    EntryCopies { copies }
  }

  /// The source of the innermost of these copies whose tree holds `dst`, as that copy brings it; the head's, for a path
  /// in none.
  pub(super) fn source_at<'g>(&self, green: &'g Green, dst: &str) -> SourceAt<'g>
  where
    'a: 'g,
  {
    let innermost = self
      .copies
      .iter()
      .filter(|(at, ..)| in_tree(dst, at))
      .max_by_key(|(at, ..)| at.len());
    match innermost {
      Some((_, from, fix)) => SourceAt {
        green,
        source: from,
        fix: *fix,
      },
      None => SourceAt {
        green,
        source: "",
        fix: None,
      },
    }
  }
}
