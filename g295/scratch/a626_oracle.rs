/// The attribute every file of the oracle's histories carries.
const NAME: &str = "n";

/// The paths the oracle's histories name: the root (as a copy's source only) first, then paths that are prefixes of one
/// another, siblings sharing a first byte, and paths often empty, so copies land and later steps change their trees.
const PATHS: [&str; 11] = [
  "", "a", "a/x", "a/x/y", "a/b", "b", "b/x", "ab", "c", "c/x", "d",
];

/// Where beneath a copy's destination a second copy of its increment lands (A-626): names the paths' trees hold, and
/// one they never hold.
const BENEATH: [&str; 4] = ["x", "x/y", "b", "m"];

/// Any path of [`PATHS`], the root among them: a copy's source or destination.
fn any_path(index: usize) -> String {
  PATHS
    .get(index % PATHS.len())
    .copied()
    .unwrap_or_default()
    .to_owned()
}

/// A path of [`PATHS`] other than the root: where a file or a directory is put.
fn named_path(index: usize) -> String {
  PATHS
    .get(1 + index % (PATHS.len() - 1))
    .copied()
    .unwrap_or_default()
    .to_owned()
}

/// A place of [`BENEATH`].
fn beneath(index: usize) -> &'static str {
  BENEATH
    .get(index % BENEATH.len())
    .copied()
    .unwrap_or_default()
}

/// One generated step of the oracle (module docs).
#[derive(Clone, Debug)]
enum Step {
  /// A one-byte file and its attribute at a path (an overwrite where a file stands).
  Put { path: usize, byte: u8 },
  /// A file and its attribute taken away.
  Take { path: usize },
  /// A directory with its mode.
  Mkdir { path: usize, mode: u8 },
  /// A tree copied, from the head or a version or two before it, replacing what its destination holds or not (A-626).
  Copy {
    from: usize,
    to: usize,
    back: u8,
    replace: bool,
  },
  /// Two copies in one increment, the second landing beneath the first's destination, each replacing or not (A-626).
  Nested {
    from: usize,
    to: usize,
    inner: usize,
    at: usize,
    replace: (bool, bool),
  },
  /// A tree copied with one of the files it brings moved within it as a stock client moves one: the file's source copied
  /// to its new place, and the file removed (A-626).
  Move {
    from: usize,
    to: usize,
    file: usize,
    at: usize,
  },
}

/// The steps, weighted by a selector over plain tuples (`prop_oneof!`'s strategy is shared-ownership backed, R2).
fn step() -> impl Strategy<Value = Step> {
  (
    0u8..16,
    (any::<usize>(), any::<usize>(), any::<usize>(), any::<usize>()),
    (any::<u8>(), any::<bool>(), any::<bool>()),
  )
    .prop_map(
      |(selector, (first, second, third, fourth), (byte, flag, other))| match selector {
        0..4 => Step::Put { path: first, byte },
        4 => Step::Take { path: first },
        5 => Step::Mkdir {
          path: first,
          mode: byte,
        },
        6..11 => Step::Copy {
          from: first,
          to: second,
          back: byte,
          replace: flag,
        },
        11..14 => Step::Nested {
          from: first,
          to: second,
          inner: third,
          at: fourth,
          replace: (flag, other),
        },
        _ => Step::Move {
          from: first,
          to: second,
          file: third,
          at: fourth,
        },
      },
    )
}

/// What the generated histories met, so the oracle shows it compared real copies.
#[derive(Clone, Copy, Debug, Default)]
struct Census {
  accepted: usize,
  refused: usize,
  from_root: usize,
  from_before: usize,
  into_themselves: usize,
  changed_after: usize,
  /// Puts and takes of a path a copy's tree holds: a key's change after its copy.
  changed_beneath: usize,
  /// Replaces accepted over a destination that held something (A-626).
  replaced: usize,
  /// Increments of two nested copies accepted (A-626).
  nested: usize,
  /// Of those, ones whose inner copy replaced what the outer brought there.
  nested_replaced: usize,
  /// Moves accepted inside a copied tree (A-626).
  moved: usize,
}

impl Census {
  /// Counts one single copy's outcome: `held`, whether its destination held anything; `changed`, whether its source's
  /// tree changed after its version.
  fn count(&mut self, copy: &Declared, accepted: bool, (head, held, changed): (u64, bool, bool)) {
    if !accepted {
      self.refused += 1;
      self.changed_after += usize::from(changed);
      return;
    }
    self.accepted += 1;
    self.from_root += usize::from(copy.src.is_empty());
    self.from_before += usize::from(copy.version < head);
    self.into_themselves += usize::from(in_tree(&copy.dst, &copy.src));
    self.replaced += usize::from(copy.replace && held);
  }
}

/// A copy one step declares: its destination, its source, the source's version, and whether it replaces (A-626).
#[derive(Clone, Debug)]
struct Declared {
  dst: String,
  src: String,
  version: u64,
  replace: bool,
}

/// One version's tree in the oracle's model: what a green reads at it.
#[derive(Clone, Debug, Default)]
struct State {
  files: BTreeMap<String, Vec<u8>>,
  dirs: BTreeSet<String>,
  modes: BTreeMap<String, u32>,
  xattrs: BTreeMap<(String, String), Vec<u8>>,
}

impl State {
  /// Every path a key is at.
  fn paths(&self) -> BTreeSet<String> {
    self
      .files
      .keys()
      .chain(&self.dirs)
      .chain(self.modes.keys())
      .chain(self.xattrs.keys().map(|(path, _)| path))
      .cloned()
      .collect()
  }

  /// Whether a key lies in `root`'s tree.
  fn holds_tree(&self, root: &str) -> bool {
    self.paths().iter().any(|path| in_tree(path, root))
  }

  /// `root`'s tree, every key moved to its place in `to`'s.
  fn tree_at(&self, root: &str, to: &str) -> State {
    let place = |path: &str| relative(path, root).map(|rel| joined(to, rel));
    State {
      files: self
        .files
        .iter()
        .filter_map(|(path, bytes)| Some((place(path)?, bytes.clone())))
        .collect(),
      dirs: self.dirs.iter().filter_map(|path| place(path)).collect(),
      modes: self
        .modes
        .iter()
        .filter_map(|(path, mode)| Some((place(path)?, *mode)))
        .collect(),
      xattrs: self
        .xattrs
        .iter()
        .filter_map(|((path, name), value)| Some(((place(path)?, name.clone()), value.clone())))
        .collect(),
    }
  }

  /// Takes `root`'s tree away; the paths it took.
  fn remove_tree(&mut self, root: &str) -> BTreeSet<String> {
    let taken: BTreeSet<String> = self
      .paths()
      .into_iter()
      .filter(|path| in_tree(path, root))
      .collect();
    self.files.retain(|path, _| !in_tree(path, root));
    self.dirs.retain(|path| !in_tree(path, root));
    self.modes.retain(|path, _| !in_tree(path, root));
    self.xattrs.retain(|(path, _), _| !in_tree(path, root));
    taken
  }

  /// Adds `tree`'s keys.
  fn graft(&mut self, tree: State) {
    self.files.extend(tree.files);
    self.dirs.extend(tree.dirs);
    self.modes.extend(tree.modes);
    self.xattrs.extend(tree.xattrs);
  }

  /// The base a green answers for this tree, in [`sorted`]'s order.
  fn base(&self) -> Base {
    Base {
      files: self
        .files
        .iter()
        .map(|(path, bytes)| (path.clone(), bytes.len() as u64))
        .collect(),
      specials: Vec::new(),
      dirs: self.dirs.iter().cloned().collect(),
      modes: self
        .modes
        .iter()
        .map(|(path, mode)| (path.clone(), *mode))
        .collect(),
      symlinks: Vec::new(),
      xattrs: self
        .xattrs
        .iter()
        .map(|((path, name), value)| (path.clone(), name.clone(), value.clone()))
        .collect(),
      hardlinks: Vec::new(),
    }
  }
}

/// `base` with every list in key order, so a green's and the model's compare whatever order each lists them in.
fn sorted(mut base: Base) -> Base {
  base.files.sort();
  base.specials.sort_by(|a, b| a.0.cmp(&b.0));
  base.dirs.sort();
  base.modes.sort();
  base.symlinks.sort();
  base.xattrs.sort();
  base.hardlinks.sort();
  base
}

/// The copy among `copies` whose tree holds `dst` (another's destination) with the longest destination: the one it
/// lands in.
fn outer_copy<'a>(copies: &'a [Declared], dst: &str) -> Option<&'a Declared> {
  copies
    .iter()
    .filter(|copy| copy.dst != dst && in_tree(dst, &copy.dst))
    .max_by_key(|copy| copy.dst.len())
}

/// What a version lands in the model: its tree, the paths it names in the change index, the destinations it copied to.
type Landed = (State, BTreeSet<String>, BTreeSet<String>);

/// The oracle's model (module docs): every version's tree, the paths each version names in the change index, and the
/// destinations of the copies each made. It states the design's rules, never the engine's code: a copy brings its
/// source's tree as the version before it held it, a replace takes its destination's tree away first, copies land in
/// their destinations' order, and a copy stands where Subversion's commit would take it.
struct Model {
  states: Vec<State>,
  named: Vec<BTreeSet<String>>,
  copied: Vec<BTreeSet<String>>,
}

impl Default for Model {
  fn default() -> Model {
    Model {
      states: vec![State::default()],
      named: vec![BTreeSet::new()],
      copied: vec![BTreeSet::new()],
    }
  }
}

impl Model {
  fn head(&self) -> u64 {
    u64::try_from(self.states.len().saturating_sub(1)).unwrap_or(u64::MAX)
  }

  fn state(&self, version: u64) -> Option<&State> {
    self.states.get(usize::try_from(version).ok()?)
  }

  fn current(&self) -> State {
    self.states.last().cloned().unwrap_or_default()
  }

  fn push(&mut self, (state, named, copied): Landed) {
    self.states.push(state);
    self.named.push(named);
    self.copied.push(copied);
  }

  /// The paths the versions in `(from, to]` name.
  fn named_between(&self, from: u64, to: u64) -> BTreeSet<String> {
    let index = |version: u64| usize::try_from(version).unwrap_or(usize::MAX);
    self
      .named
      .iter()
      .take(index(to).saturating_add(1))
      .skip(index(from).saturating_add(1))
      .flatten()
      .cloned()
      .collect()
  }

  /// Whether `root`'s tree changed after `version`: a later version named a path in it, or copied onto it or above it.
  fn changed_after(&self, root: &str, version: u64) -> bool {
    let after = usize::try_from(version).map_or(usize::MAX, |version| version.saturating_add(1));
    let named = self
      .named
      .iter()
      .skip(after)
      .flatten()
      .any(|path| in_tree(path, root));
    let replaced = self
      .copied
      .iter()
      .skip(after)
      .flatten()
      .any(|dst| in_tree(root, dst));
    named || replaced
  }

  /// Whether every copy of one increment stands (based on the head): each destination other than the root holds nothing
  /// or is replaced, a destination beneath another copy of the increment judged against what that copy brings there;
  /// each source holds something and did not change after its version.
  fn copies_stand(&self, copies: &[Declared]) -> bool {
    let head = self.current();
    copies.iter().all(|copy| {
      let place = outer_copy(copies, &copy.dst).map_or_else(
        || copy.dst.clone(),
        |outer| joined(&outer.src, relative(&copy.dst, &outer.dst).unwrap_or_default()),
      );
      let destination = !copy.dst.is_empty() && (copy.replace || !head.holds_tree(&place));
      let source = head.holds_tree(&copy.src) && !self.changed_after(&copy.src, copy.version);
      destination && source
    })
  }

  /// What one increment of `copies` and the removals `unlinked` in their trees lands: every source read from the head
  /// first, then in their destinations' order each replaced destination's tree taken away and each source's tree put
  /// there, then the removals.
  fn after_copies(&self, copies: &[Declared], unlinked: &[String]) -> Landed {
    let mut next = self.current();
    let mut order: Vec<&Declared> = copies.iter().collect();
    order.sort_by(|a, b| a.dst.cmp(&b.dst));
    let sources: Vec<State> = order
      .iter()
      .map(|copy| next.tree_at(&copy.src, &copy.dst))
      .collect();
    let mut named: BTreeSet<String> = order.iter().map(|copy| copy.dst.clone()).collect();
    let copied = named.clone();
    for (copy, source) in order.into_iter().zip(sources) {
      if copy.replace {
        named.extend(next.remove_tree(&copy.dst));
      }
      next.graft(source);
    }
    for path in unlinked {
      next.files.remove(path);
      next.xattrs.remove(&(path.clone(), NAME.to_owned()));
      named.insert(path.clone());
    }
    (next, named, copied)
  }
}

/// The green under test, the model it is held to, and what each accepted version applied.
#[derive(Default)]
struct Oracle {
  green: Green,
  model: Model,
  entries: Vec<(Increment, Vec<chain_entry::Applied>)>,
  next: u64,
}

impl Oracle {
  /// Applies one step to the green, which must decide it as the model does.
  fn apply(&mut self, step: &Step, census: &mut Census) -> Result<(), TestCaseError> {
    match *step {
      Step::Put { path, byte } => self.put(&named_path(path), byte, census),
      Step::Take { path } => self.take(&named_path(path), census),
      Step::Mkdir { path, mode } => self.mkdir(&named_path(path), u32::from(mode)),
      Step::Copy {
        from,
        to,
        back,
        replace,
      } => self.copy_step((from, to), back, replace, census),
      Step::Nested {
        from,
        to,
        inner,
        at,
        replace,
      } => self.nested_step((from, to), (inner, at), replace, census),
      Step::Move { from, to, file, at } => self.move_step((from, to), (file, at), census),
    }
  }

  /// Submits `work` at the head, which the model expects accepted with what it lands (`Some`) or refused; the green must
  /// agree, and an accepted version's entry is kept for the replay. Whether it was accepted.
  fn submit(&mut self, work: &Work, expected: Option<Landed>) -> Result<bool, TestCaseError> {
    self.next += 1;
    let increment = work.at(self.next, self.green.head());
    let (outcome, applied) = self.green.submit_resolved(&increment);
    let accepted = matches!(outcome, Outcome::Accepted { .. });
    prop_assert_eq!(accepted, expected.is_some(), "{:?}", outcome);
    if let (Some(applied), Some(landed)) = (applied, expected) {
      self.entries.push((increment, applied));
      self.model.push(landed);
    }
    Ok(accepted)
  }

  /// A one-byte file at `path` and its attribute: an overwrite where a file stands, refused where a directory does.
  fn put(&mut self, path: &str, byte: u8, census: &mut Census) -> Result<(), TestCaseError> {
    census.changed_beneath += usize::from(self.green.copy_above(path).is_some());
    let mut next = self.model.current();
    let stands = next.files.contains_key(path) || !next.dirs.contains(path);
    let mut work = Work::default();
    let bytes = match next.files.get(path) {
      Some(old) => {
        work.overwrite(path, 0, &[byte]);
        let mut bytes = old.clone();
        bytes.splice(..1.min(bytes.len()), [byte]);
        bytes
      }
      None => {
        work.create(path, &[byte]);
        vec![byte]
      }
    };
    work.xattr(path, NAME, &[byte]);
    next.files.insert(path.to_owned(), bytes);
    next.xattrs.insert((path.to_owned(), NAME.to_owned()), vec![byte]);
    let named = BTreeSet::from([path.to_owned()]);
    self
      .submit(&work, stands.then_some((next, named, BTreeSet::new())))
      .map(drop)
  }

  /// The file at `path` and its attribute taken away, where a file stands.
  fn take(&mut self, path: &str, census: &mut Census) -> Result<(), TestCaseError> {
    let mut next = self.model.current();
    if next.files.remove(path).is_none() {
      return Ok(());
    }
    census.changed_beneath += usize::from(self.green.copy_above(path).is_some());
    let mut work = Work::default();
    work.unlink(path);
    if next
      .xattrs
      .remove(&(path.to_owned(), NAME.to_owned()))
      .is_some()
    {
      work.remove_xattr(path, NAME);
    }
    let named = BTreeSet::from([path.to_owned()]);
    self
      .submit(&work, Some((next, named, BTreeSet::new())))
      .map(drop)
  }

  /// A directory at `path` with `mode`, refused where a file stands.
  fn mkdir(&mut self, path: &str, mode: u32) -> Result<(), TestCaseError> {
    let mut next = self.model.current();
    let stands = !next.files.contains_key(path);
    next.dirs.insert(path.to_owned());
    next.modes.insert(path.to_owned(), mode);
    let mut work = Work::default();
    work.mkdir(path).mode(path, mode);
    let named = BTreeSet::from([path.to_owned()]);
    self
      .submit(&work, stands.then_some((next, named, BTreeSet::new())))
      .map(drop)
  }

  /// One increment of `copies` and the removals `unlinked` in their trees, node-exclusively: the model decides and lands
  /// it, and the green must agree; an increment that replaces nothing walks no map. Whether it was accepted.
  fn copies(&mut self, copies: &[Declared], unlinked: &[String]) -> Result<bool, TestCaseError> {
    let mut work = Work::exclusive();
    for copy in copies {
      work.declared(copy);
    }
    for path in unlinked {
      work.unlink(path).remove_xattr(path, NAME);
    }
    let expected = self
      .model
      .copies_stand(copies)
      .then(|| self.model.after_copies(copies, unlinked));
    let walked = self.green.walked();
    let accepted = self.submit(&work, expected)?;
    if accepted && copies.iter().all(|copy| !copy.replace) {
      prop_assert_eq!(self.green.walked(), walked, "a copy walked a map");
    }
    Ok(accepted)
  }

  /// A copy of `from` at the head less `back % 3` to `to`, replacing or not.
  fn copy_step(
    &mut self,
    (from, to): (usize, usize),
    back: u8,
    replace: bool,
    census: &mut Census,
  ) -> Result<(), TestCaseError> {
    let head = self.model.head();
    let copy = Declared {
      dst: any_path(to),
      src: any_path(from),
      version: head.saturating_sub(u64::from(back % 3)),
      replace,
    };
    let held = self.model.current().holds_tree(&copy.dst);
    let changed = self.model.changed_after(&copy.src, copy.version);
    let accepted = self.copies(std::slice::from_ref(&copy), &[])?;
    census.count(&copy, accepted, (head, held, changed));
    Ok(())
  }

  /// A copy of `from` to `to` with a second copy, of `inner`, beneath it at `at`, both of the head's version.
  fn nested_step(
    &mut self,
    (from, to): (usize, usize),
    (inner, at): (usize, usize),
    replace: (bool, bool),
    census: &mut Census,
  ) -> Result<(), TestCaseError> {
    let head = self.model.head();
    let outer = Declared {
      dst: any_path(to),
      src: any_path(from),
      version: head,
      replace: replace.0,
    };
    let place = beneath(at);
    let filled = self
      .model
      .current()
      .holds_tree(&joined(&outer.src, place));
    let inner = Declared {
      dst: joined(&outer.dst, place),
      src: any_path(inner),
      version: head,
      replace: replace.1,
    };
    let accepted = self.copies(&[outer, inner], &[])?;
    census.nested += usize::from(accepted);
    census.nested_replaced += usize::from(accepted && filled && replace.1);
    Ok(())
  }

  /// A copy of `from` to `to` with one of the files it brings moved to `at` beneath it, as a stock client moves one: the
  /// file's source copied there, the file removed. Nothing where `from`'s tree holds no file.
  fn move_step(
    &mut self,
    (from, to): (usize, usize),
    (file, at): (usize, usize),
    census: &mut Census,
  ) -> Result<(), TestCaseError> {
    let head = self.model.head();
    let (src, dst) = (any_path(from), any_path(to));
    let state = self.model.current();
    let files: Vec<&String> = state
      .files
      .keys()
      .filter(|path| in_tree(path, &src))
      .collect();
    let Some(moved) = files.get(file % files.len().max(1)).copied() else {
      return Ok(());
    };
    let unlinked = joined(&dst, relative(moved, &src).unwrap_or_default());
    let copies = [
      Declared {
        dst: dst.clone(),
        src: src.clone(),
        version: head,
        replace: false,
      },
      Declared {
        dst: joined(&dst, beneath(at)),
        src: moved.clone(),
        version: head,
        replace: false,
      },
    ];
    let accepted = self.copies(&copies, &[unlinked])?;
    census.moved += usize::from(accepted);
    Ok(())
  }

  /// Holds the green to the model at every version (each base, each file's bytes, the paths named between it and the
  /// head), its identity to its recount, its change index's bytes to theirs, and no copy refused by a map its verdict
  /// passed.
  fn check(&mut self) -> Result<(), TestCaseError> {
    let head = self.green.head();
    prop_assert_eq!(head, self.model.head());
    for version in 0..=head {
      self.check_version(version, head)?;
    }
    prop_assert_eq!(
      self.green.head_identity(),
      self.green.head_identity_from_scratch()
    );
    prop_assert_eq!(self.green.changes_bytes(), self.green.changes_recounted());
    prop_assert_eq!(self.green.copies_refused(), 0);
    Ok(())
  }

  /// [`Oracle::check`] at one version.
  fn check_version(&self, version: u64, head: u64) -> Result<(), TestCaseError> {
    let state = self
      .model
      .state(version)
      .ok_or_else(|| TestCaseError::fail("a version the model lacks"))?;
    prop_assert_eq!(
      sorted(self.green.base_at(version)),
      state.base(),
      "the base at {}",
      version
    );
    for (path, bytes) in &state.files {
      prop_assert_eq!(
        self.green.content_at(path, version).as_ref(),
        Some(bytes),
        "{} at {}",
        path,
        version
      );
    }
    let named: BTreeSet<String> = self
      .green
      .changed_between(version, head)
      .into_iter()
      .collect();
    prop_assert_eq!(
      named,
      self.model.named_between(version, head),
      "the paths named since {}",
      version
    );
    Ok(())
  }

  /// Every accepted version's chain entry, encoded and decoded, replayed on a fresh green; with the replica's identity at
  /// each version.
  fn replay(&self) -> Result<(Green, Vec<[u8; 32]>), TestCaseError> {
    let mut replica = Green::new();
    let mut identities = vec![replica.head_identity()];
    for (increment, applied) in &self.entries {
      let entry = chain_entry::encode(&increment.encode(), increment, applied, None, None, None);
      let decoded =
        chain_entry::decode(&entry).map_err(|error| TestCaseError::fail(format!("{error:?}")))?;
      replica.apply_recorded(&decoded.increment, &decoded.applied);
      identities.push(replica.head_identity());
    }
    Ok((replica, identities))
  }

  /// The replay must read as the green at every version and hash as it; then a branch of the green at every version
  /// must read as the model's tree there and hash as the replay did, and no branch may walk a map.
  fn replay_and_branch(mut self) -> Result<(), TestCaseError> {
    let (replica, identities) = self.replay()?;
    let head = self.green.head();
    for version in 0..=head {
      prop_assert_eq!(
        replica.base_at(version),
        self.green.base_at(version),
        "the replay at {}",
        version
      );
    }
    prop_assert_eq!(identities.last().copied(), Some(self.green.head_identity()));
    let Oracle { green, model, .. } = self;
    let (mut lineage, source) = Lineage::new(green);
    let walked = lineage.walked();
    let refused = |refused: super::LineageRefused| TestCaseError::fail(format!("{refused:?}"));
    for (version, identity) in (0..=head).zip(identities) {
      let branch = lineage.branch(source, version).map_err(refused)?;
      let (base, hashed) = lineage
        .with(branch, |green| (green.base_at(0), green.head_identity()))
        .map_err(refused)?;
      let state = model
        .state(version)
        .ok_or_else(|| TestCaseError::fail("a version the model lacks"))?;
      prop_assert_eq!(sorted(base), state.base(), "the branch at {}", version);
      prop_assert_eq!(hashed, identity, "the branch's identity at {}", version);
    }
    prop_assert_eq!(lineage.walked(), walked, "a branch walked a map");
    Ok(())
  }
}

/// Runs one history, checking after every step, then replays the green's chain and branches it at every version.
fn run(steps: &[Step], census: &mut Census) -> Result<(), TestCaseError> {
  let mut oracle = Oracle::default();
  for step in steps {
    oracle.apply(step, census)?;
    oracle.check()?;
  }
  oracle.replay_and_branch()
}

/// A-586 and A-626 (the oracle). Do: generated histories of files and their attributes put and taken, directories made
/// with modes, single copies of any path (the root among them) at the head or a version or two before it to any path
/// (into the source's own tree among them), replacing what the destination holds or not, two copies nested in one
/// increment, and moves inside a copied tree as a stock client makes them, on a green and on a model that states every
/// version's tree. Expect: the green decides every step as the model does; after every step it reads as the model at
/// every version (each base, each file's bytes, the paths the change index names since), its identity equals its
/// recount and its change index's bytes theirs, no map refused a copy its verdict passed, and a copy that replaces
/// nothing walked no map; at the end its chain, replayed from its entries, reads and hashes as it does, and a branch at
/// every version reads as the model and hashes as the replay, walking no map. The census shows each kind of copy met.
#[test]
fn every_copy_reads_as_its_model_over_every_history() {
  let census = RefCell::new(Census::default());
  let config = Config {
    cases: slates_test_seeds::cases(128),
    ..Config::default()
  };
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(config));
  let result = runner.run(&prop::collection::vec(step(), 1..24), |steps| {
    run(&steps, &mut census.borrow_mut())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  // The figures the evidence quotes (`-- --nocapture`).
  eprintln!("copy oracle census: {census:?}");
  let met = [
    census.accepted,
    census.refused,
    census.from_root,
    census.from_before,
    census.into_themselves,
    census.changed_after,
    census.changed_beneath,
    census.replaced,
    census.nested,
    census.nested_replaced,
    census.moved,
  ];
  assert!(
    met.iter().all(|&count| count > 0),
    "a case never met: {census:?}"
  );
}
