//! A green made at another green's version without a walk (§4.16 "A green made at another green's version"; §4.17.1
//! item 4 and shared piece 1's second half; A-534).
//!
//! **What it is.** A [`Lineage`] holds greens made from one another's versions — a repository's branches (§4.17.1) —
//! and the arenas their persistent maps share ([`crate::dims`]). [`Lineage::branch`] makes a green whose version 0 is
//! version `v` of another: it forks each of the other's eight maps (one count each), then sets back, in the new green
//! alone, every key that changed after `v` to its value at `v`, read from the other's histories, and moves the other's
//! head identity to `v` by the same keys (the identity is a set hash, `crate::identity`: an entry out, an entry in). The
//! new green's histories, last-change indexes and change index start empty: by the engine's one history rule every key
//! has held its value since version 0. It answers every read as a green seeded from a copy of the other's state at `v`
//! does ([`Green::with_origin`], the copying reference its tests hold it to). A copy the other made after `v` (A-586) is
//! taken away first, its tree cut from the new green's maps in O(its destination's length) and its entry out of the
//! identity, with the entry it gave each key that changed after it back in. Every key the change index names is read at
//! `v` as the green's readers read it, the copies above it counted, and set back where that differs from what the new
//! green holds once the cuts are made: a key of a cut tree that held a value at `v` is set back whatever the head holds
//! (A-626; until then a key equal at both ends was left cut, which a replace that gives a tree back meets, and a key read
//! at `v` from its own history alone, before a copy at or before `v` that gave it its value, was set to the older
//! value). A copy made at or before `v` is part of the
//! state the new green starts from: its keys hold their values since version 0 there, and its entry stays in the
//! identity's sum, which a recount of the new green's own state does not reproduce (its records stay with the other).
//!
//! **What it costs.** Let `E` be the change index's entries in `(v, head]`, `c` the paths they name, `L` the longest
//! such path in bytes, `n` the keys a history map holds and `B` the bytes of the changed files at `v` and at the head.
//! A branch costs O(E + c·(L + log n) + B): the paths changed after `v` are read from the change index, each one's
//! histories are looked up, each key set back copies at most one node per byte of its path, and each file set back is
//! read out of the history's chunks once and hashed into the identity twice (out at the head, in at `v`). Nothing in it
//! grows with the tree: a branch at the head is O(1) (eight counts and a copy of the identity's sum), and one at an
//! older version is proportional to what changed since. Bringing the other's identity to its head first costs the keys
//! it changed since the identity was last read, which any read of it pays.
//!
//! **What it never does.** It walks no map: every node a walk visits is counted ([`Lineage::walked`]), and a branch moves
//! that count by nothing, the non-vacuity count its tests assert. A version below the other's fold floor (or the
//! checkpoint it was restored at) is refused, typed: its values are no longer reconstructible
//! ([`LineageRefused::Folded`]).
//!
//! **One owner of every node.** The lineage owns the arenas and its greens, and lends the arenas to one green for the
//! length of one call ([`Lineage::with`]); a green never leaves the lineage holding nodes of it (CLAUDE.md R2: no
//! shared-ownership pointer). A retired green gives its holds back, freeing what no other green shares
//! ([`Lineage::retire`]); every green's handle carries its seat's generation, so a retired green's handle is a typed
//! miss. The seats are the greens the lineage's owner admitted, a seat freed by a retire is reused first.
//!
//! **Laptop ≡ fleet.** A lineage lives on the shard that owns the repository's greens (§4.17.1 item 2), on a laptop and
//! in a fleet alike; a holder replays a branch from the same record the owner made it from (owed with shared piece 2).

use std::borrow::Cow;
use std::ops::{Deref, DerefMut};

use super::{
  Green, IDENTITY_TAG_HARDLINK, IDENTITY_TAG_SYMLINK, IdentityKey, dir_entry, file_entry,
  link_entry, mode_entry, relink, special_entry, xattr_entry,
};
use crate::dims::{Arenas, GreenRelease};
use crate::identity::{Entry, SetHash};
use crate::special::SpecialNode;

/// One key a branch sets back to its value at the version it was made at (`None`: absent then).
#[derive(Debug)]
enum Revert {
  File(String, Option<Vec<u8>>),
  Special(String, Option<SpecialNode>),
  Dir(String, bool),
  Mode(String, Option<u32>),
  Symlink(String, Option<String>),
  Hardlink(String, Option<String>),
  Xattr(String, String, Option<Vec<u8>>),
  /// A copy's tree taken away, made after the version (A-586): applied before every other revert, so the keys set back
  /// beneath it land where it stood.
  Cut(String),
}

/// Why a lineage refused a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineageRefused {
  /// The handle names no green of this lineage: retired, or never this lineage's.
  Stale,
  /// The version is past the green's head.
  PastHead {
    /// The version asked for.
    version: u64,
    /// The green's head.
    head: u64,
  },
  /// The version is below the oldest the green still reconstructs: its fold floor, or the checkpoint it was restored
  /// at.
  Folded {
    /// The version asked for.
    version: u64,
    /// The oldest version the green reconstructs.
    floor: u64,
  },
  /// Another member's engine is out of the lineage with the arenas in it ([`Lineage::take`], [`Lent`]), so no other
  /// member can be read, written, branched or released until it is put back (A-575).
  Busy,
}

/// Whether one dimension's key at a path has a history of its own, with the key it names there
/// ([`Green::keys_with_history`]).
type HasHistory = (bool, fn(String) -> IdentityKey);

/// What a branch does to one key (module docs): its value at the version (`then`, read as the green's readers read it)
/// against the head's (`now`); the identity's entry exchanged when they differ, and the key set back when `then`
/// differs from what the branch holds once its cuts are made, nothing for a key in a tree a cut took away (`cut`). The
/// value to set back, when one is owed.
fn set_back<T: PartialEq>(
  identity: &mut SetHash,
  (then, now): (Option<T>, Option<&T>),
  cut: bool,
  entry: impl Fn(&T) -> Entry,
) -> Option<Option<T>> {
  if then.as_ref() != now {
    exchange(identity, now.map(&entry), then.as_ref().map(&entry));
  }
  let held = now.filter(|_| !cut);
  (then.as_ref() != held).then_some(then)
}

/// Moves `identity` from an entry to another: the head's out, the version's in (either absent).
fn exchange(identity: &mut SetHash, out: Option<Entry>, into: Option<Entry>) {
  if let Some(out) = out {
    identity.remove(&out);
  }
  if let Some(into) = into {
    identity.add(&into);
  }
}

impl Green {
  /// Exchanges every map's arena with `arenas`'s (`crate::dims`, "Arenas are lent").
  fn swap_arenas(&mut self, arenas: &mut Arenas) {
    self
      .content
      .swap_arena(&mut arenas.files, &mut arenas.blocks);
    self.specials.swap_arena(&mut arenas.specials);
    self.dirs.swap_arena(&mut arenas.dirs);
    self.modes.swap_arena(&mut arenas.modes);
    self.symlinks.swap_arena(&mut arenas.symlinks);
    self.hardlinks.swap_arena(&mut arenas.hardlinks);
    self.xattrs.swap_arena(&mut arenas.xattrs);
    self.links_to.swap_arena(&mut arenas.links_to);
  }

  /// Exchanges every map's arena with `other`'s.
  fn lend_arenas(&mut self, other: &mut Green) {
    self.content.lend(&mut other.content);
    self.specials.lend(&mut other.specials);
    self.dirs.lend(&mut other.dirs);
    self.modes.lend(&mut other.modes);
    self.symlinks.lend(&mut other.symlinks);
    self.hardlinks.lend(&mut other.hardlinks);
    self.xattrs.lend(&mut other.xattrs);
    self.links_to.lend(&mut other.links_to);
  }

  /// Every map's hold on its nodes, to give back a node a step in the lineage's arenas ([`Lineage::release_step`]):
  /// the maps are empty after, and nothing is freed until the steps run (A-575).
  fn releasing(&mut self) -> GreenRelease {
    GreenRelease {
      files: self.content.releasing(),
      specials: self.specials.releasing(),
      dirs: self.dirs.releasing(),
      modes: self.modes.releasing(),
      symlinks: self.symlinks.releasing(),
      hardlinks: self.hardlinks.releasing(),
      xattrs: self.xattrs.releasing(),
      links_to: self.links_to.releasing(),
    }
  }

  /// The green whose version 0 is this one's `version` (module docs), sharing this one's arenas, which it holds for
  /// this call: its maps forked from this one's, every key changed after `version` set back, and its identity this one's
  /// at `version`. Also the keys it set back.
  fn branch_at(&mut self, version: u64) -> Result<(Green, u64), LineageRefused> {
    let head = self.head();
    if version > head {
      return Err(LineageRefused::PastHead { version, head });
    }
    let floor = self.folded_below.max(self.deltas_from);
    if version < floor {
      return Err(LineageRefused::Folded { version, floor });
    }
    self.fold_identity();
    let mut identity = self.identity.clone();
    let reverts = self.reverts(version, &mut identity);
    let mut branch = Green {
      content: self.content.fork(),
      specials: self.specials.fork(),
      dirs: self.dirs.fork(),
      modes: self.modes.fork(),
      symlinks: self.symlinks.fork(),
      hardlinks: self.hardlinks.fork(),
      xattrs: self.xattrs.fork(),
      links_to: self.links_to.fork(),
      identity,
      ..Green::default()
    };
    let reverted = u64::try_from(reverts.len()).unwrap_or(u64::MAX);
    // The arenas are this green's for the call: lent to the branch while it sets its keys back, then returned.
    branch.lend_arenas(self);
    for revert in reverts {
      branch.revert(revert);
    }
    branch.lend_arenas(self);
    Ok((branch, reverted))
  }

  /// Every key whose value at `version` differs from what the new green holds once its cuts are made (the head's, or
  /// nothing in a cut tree), as the revert that sets it back, read for the paths the change index names in
  /// `(version, head]`; `identity` (this green's at its head) is moved to `version` by the keys whose value differs
  /// from the head's.
  fn reverts(&self, version: u64, identity: &mut SetHash) -> Vec<Revert> {
    let changed = self.changed_between(version, self.head());
    let mut reverts = self.copy_reverts(&changed, version, identity);
    for path in &changed {
      // A copy made after the version takes its whole tree away, keys equal at both ends among them (A-626).
      let cut = !self.copies_above_after(path, version).is_empty();
      self.file_reverts(path, (version, cut), identity, &mut reverts);
      self.node_reverts(path, (version, cut), identity, &mut reverts);
      self.link_reverts(path, (version, cut), identity, &mut reverts);
      self.xattr_reverts(path, (version, cut), identity, &mut reverts);
      self.copied_reverts(path, version, identity);
    }
    reverts
  }

  /// [`Green::reverts`] of the copies made after `version` (A-586), first: each one's tree cut and its entry taken out
  /// of `identity`, which then no longer stands for the tree's entries. The change index names each copy's destination at
  /// its version, so this costs the paths it names.
  fn copy_reverts(&self, changed: &[String], version: u64, identity: &mut SetHash) -> Vec<Revert> {
    let mut reverts = Vec::new();
    for dst in changed {
      let entries = self.copy_entries_after(dst, version);
      if entries.is_empty() {
        continue;
      }
      for entry in &entries {
        identity.remove(entry);
      }
      reverts.push(Revert::Cut(dst.clone()));
    }
    reverts
  }

  /// [`Green::reverts`] of the entries copies made after `version` gave `path`'s keys (A-586): put back into `identity`
  /// for every key there with a history of its own, since that key's exchange takes out its head entry (or, equal at
  /// both ends, leaves its entry at `version` standing), and the copy's entry, taken out, stood for the one it gave. A
  /// key with no history at a path a copy after `version` covers held nothing then and holds the copy's value now, so it
  /// moves nothing. Until A-626 only keys with a change after `version` were counted, which missed a copy's own
  /// destination when its history ended before the copy (removed, then copied over).
  fn copied_reverts(&self, path: &str, version: u64, identity: &mut SetHash) {
    if self.copies_above_after(path, version).is_empty() {
      return;
    }
    for key in self.keys_with_history(path) {
      for entry in self.copied_entries(&key, version) {
        identity.add(&entry);
      }
    }
  }

  /// `path`'s keys, in every dimension, that have a history of their own.
  fn keys_with_history(&self, path: &str) -> Vec<IdentityKey> {
    let recorded: &[HasHistory] = &[
      (self.content_history.contains_key(path), IdentityKey::File),
      (
        self.special_history.contains_key(path),
        IdentityKey::Special,
      ),
      (self.dir_history.contains_key(path), IdentityKey::Dir),
      (self.mode_history.contains_key(path), IdentityKey::Mode),
      (
        self.symlink_history.contains_key(path),
        IdentityKey::Symlink,
      ),
      (
        self.hardlink_history.contains_key(path),
        IdentityKey::Hardlink,
      ),
    ];
    let mut keys: Vec<IdentityKey> = recorded
      .iter()
      .filter(|(held, _)| *held)
      .map(|(_, key)| key(path.to_owned()))
      .collect();
    let from = (path.to_owned(), String::new());
    keys.extend(
      self
        .xattr_history
        .range(from..)
        .take_while(|((at, _), _)| at == path)
        .map(|((_, name), _)| IdentityKey::Xattr(path.to_owned(), name.clone())),
    );
    keys
  }

  /// [`Green::reverts`] of `path`'s bytes. `cut`: a copy made after `version` took `path`'s tree away.
  fn file_reverts(
    &self,
    path: &str,
    (version, cut): (u64, bool),
    identity: &mut SetHash,
    reverts: &mut Vec<Revert>,
  ) {
    if !self.content_history.contains_key(path) {
      return;
    }
    let then = self.content_ref_at(path, version);
    let now = self.content.get(path);
    if then.as_deref() != now {
      exchange(
        identity,
        now.map(|bytes| file_entry(path, bytes)),
        then.as_deref().map(|bytes| file_entry(path, bytes)),
      );
    }
    if then.as_deref() != now.filter(|_| !cut) {
      reverts.push(Revert::File(path.to_owned(), then.map(Cow::into_owned)));
    }
  }

  /// [`Green::reverts`] of `path`'s IPC node, directory and mode.
  fn node_reverts(
    &self,
    path: &str,
    (version, cut): (u64, bool),
    identity: &mut SetHash,
    reverts: &mut Vec<Revert>,
  ) {
    if self.special_history.contains_key(path) {
      let pair = (self.special_at(path, version), self.specials.get(path));
      if let Some(then) = set_back(identity, pair, cut, |node| special_entry(path, node)) {
        reverts.push(Revert::Special(path.to_owned(), then));
      }
    }
    if self.dir_history.contains_key(path) {
      let (then, now) = (self.dir_at(path, version), self.dirs.contains_key(path));
      if then != now {
        exchange(
          identity,
          now.then(|| dir_entry(path)),
          then.then(|| dir_entry(path)),
        );
      }
      if then != (now && !cut) {
        reverts.push(Revert::Dir(path.to_owned(), then));
      }
    }
    if self.mode_history.contains_key(path) {
      let pair = (self.mode_at(path, version), self.modes.get(path));
      if let Some(then) = set_back(identity, pair, cut, |mode| mode_entry(path, *mode)) {
        reverts.push(Revert::Mode(path.to_owned(), then));
      }
    }
  }

  /// [`Green::reverts`] of `path`'s symbolic link and hard link.
  fn link_reverts(
    &self,
    path: &str,
    (version, cut): (u64, bool),
    identity: &mut SetHash,
    reverts: &mut Vec<Revert>,
  ) {
    let symlink = |target: &String| link_entry(IDENTITY_TAG_SYMLINK, path, target);
    if self.symlink_history.contains_key(path) {
      let pair = (self.symlink_at(path, version), self.symlinks.get(path));
      if let Some(then) = set_back(identity, pair, cut, symlink) {
        reverts.push(Revert::Symlink(path.to_owned(), then));
      }
    }
    let hardlink = |target: &String| link_entry(IDENTITY_TAG_HARDLINK, path, target);
    if self.hardlink_history.contains_key(path) {
      let pair = (self.hardlink_at(path, version), self.hardlinks.get(path));
      if let Some(then) = set_back(identity, pair, cut, hardlink) {
        reverts.push(Revert::Hardlink(path.to_owned(), then));
      }
    }
  }

  /// [`Green::reverts`] of `path`'s attributes: every name its attribute history holds.
  fn xattr_reverts(
    &self,
    path: &str,
    (version, cut): (u64, bool),
    identity: &mut SetHash,
    reverts: &mut Vec<Revert>,
  ) {
    let from = (path.to_owned(), String::new());
    for ((_, name), _) in self
      .xattr_history
      .range(from..)
      .take_while(|((at, _), _)| at == path)
    {
      let pair = (
        self.xattr_at(path, name, version),
        self.xattrs.get(path, name),
      );
      let entry = |value: &Vec<u8>| xattr_entry(path, name, value);
      if let Some(then) = set_back(identity, pair, cut, entry) {
        reverts.push(Revert::Xattr(path.to_owned(), name.clone(), then));
      }
    }
  }

  /// Sets one key back: the map's write, and the reverse map of hard links with a link's.
  fn revert(&mut self, revert: Revert) {
    match revert {
      Revert::File(path, Some(bytes)) => self.content.insert(&path, bytes),
      Revert::File(path, None) => {
        self.content.remove(&path);
      }
      Revert::Special(path, Some(node)) => {
        self.specials.insert(&path, node);
      }
      Revert::Special(path, None) => {
        self.specials.remove(&path);
      }
      Revert::Dir(path, true) => {
        self.dirs.insert(&path, ());
      }
      Revert::Dir(path, false) => {
        self.dirs.remove(&path);
      }
      Revert::Mode(path, Some(mode)) => {
        self.modes.insert(&path, mode);
      }
      Revert::Mode(path, None) => {
        self.modes.remove(&path);
      }
      Revert::Symlink(path, Some(target)) => {
        self.symlinks.insert(&path, target);
      }
      Revert::Symlink(path, None) => {
        self.symlinks.remove(&path);
      }
      Revert::Hardlink(path, target) => {
        let old = match &target {
          Some(target) => self.hardlinks.insert(&path, target.clone()),
          None => self.hardlinks.remove(&path),
        };
        relink(&mut self.links_to, &path, old.as_deref(), target.as_deref());
      }
      Revert::Xattr(path, name, Some(value)) => {
        self.xattrs.insert(&path, &name, value);
      }
      Revert::Xattr(path, name, None) => {
        self.xattrs.remove(&path, &name);
      }
      Revert::Cut(dst) => {
        // The reverse map of hard links holds nothing a copy brought: a copy's source holds no link (A-586).
        self.content.cut_tree(&dst);
        self.specials.cut_tree(&dst);
        self.dirs.cut_tree(&dst);
        self.modes.cut_tree(&dst);
        self.symlinks.cut_tree(&dst);
        self.hardlinks.cut_tree(&dst);
        self.xattrs.cut_tree(&dst);
      }
    }
  }
}

/// A seat of a [`Lineage`]: its green while it holds one, and its generation, raised by each retire.
#[derive(Debug, Default)]
struct Seat {
  generation: u64,
  green: Option<Green>,
}

/// A green of a [`Lineage`]: its seat, and the seat's generation when the green took it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineageGreen {
  seat: usize,
  generation: u64,
}

/// Greens made from one another's versions, sharing the arenas of their maps (module docs).
#[derive(Debug, Default)]
pub struct Lineage {
  /// The arenas every green's maps live in, lent to one green a call.
  arenas: Arenas,
  /// The greens, each in its seat.
  seats: Vec<Seat>,
  /// Seats a retire freed, taken first.
  free: Vec<usize>,
  /// The member whose engine is out of its seat with the arenas in it ([`Lineage::take`]): every other lend is refused
  /// `Busy` until it is put back.
  out: Option<LineageGreen>,
  /// Keys every branch set back.
  reverted: u64,
}

/// A retired green's holds on its lineage's nodes, given back a node a step ([`Lineage::release_step`]), so a release
/// as large as the green runs in its shard's slices (A-575).
#[derive(Debug, Default)]
pub struct Release {
  maps: GreenRelease,
}

impl Release {
  /// Whether every hold has been given back.
  pub fn is_done(&self) -> bool {
    self.maps.is_done()
  }
}

/// A member's engine lent out of its lineage with the arenas in it, for as long as the guard lives (A-575): it reads
/// and writes as a green standing alone does, and goes back to its seat, the arenas with it, when the guard is dropped.
/// While it is out every other lend of the lineage is refused [`LineageRefused::Busy`]. `L` is how the lineage is
/// borrowed: `&mut Lineage`, or a `RefMut` of one a shard keeps in a cell.
pub struct Lent<L: DerefMut<Target = Lineage>> {
  lineage: L,
  handle: LineageGreen,
  green: Green,
}

impl<L: DerefMut<Target = Lineage>> Lent<L> {
  /// `handle`'s engine, lent out of `lineage`.
  pub fn new(mut lineage: L, handle: LineageGreen) -> Result<Lent<L>, LineageRefused> {
    let green = lineage.take(handle)?;
    Ok(Lent {
      lineage,
      handle,
      green,
    })
  }
}

impl<L: DerefMut<Target = Lineage>> Deref for Lent<L> {
  type Target = Green;

  fn deref(&self) -> &Green {
    &self.green
  }
}

impl<L: DerefMut<Target = Lineage>> DerefMut for Lent<L> {
  fn deref_mut(&mut self) -> &mut Green {
    &mut self.green
  }
}

impl<L: DerefMut<Target = Lineage>> Drop for Lent<L> {
  fn drop(&mut self) {
    let green = std::mem::take(&mut self.green);
    self.lineage.restore(self.handle, green);
  }
}

impl Lineage {
  /// A lineage whose first green is `green`: the green's arenas become the lineage's, nothing copied.
  pub fn new(mut green: Green) -> (Lineage, LineageGreen) {
    let mut lineage = Lineage::default();
    green.swap_arenas(&mut lineage.arenas);
    let first = lineage.seat(green);
    (lineage, first)
  }

  /// A new member with nothing in it, at version 0: it shares nothing until it changes.
  pub fn create(&mut self) -> LineageGreen {
    self.seat(Green::new())
  }

  /// Seats `green`, whose maps' arenas are the lineage's.
  fn seat(&mut self, green: Green) -> LineageGreen {
    if let Some(seat) = self.free.pop()
      && let Some(place) = self.seats.get_mut(seat)
    {
      place.green = Some(green);
      return LineageGreen {
        seat,
        generation: place.generation,
      };
    }
    self.seats.push(Seat {
      generation: 0,
      green: Some(green),
    });
    LineageGreen {
      seat: self.seats.len().saturating_sub(1),
      generation: 0,
    }
  }

  /// Takes `handle`'s green out of its seat.
  fn unseat(&mut self, handle: LineageGreen) -> Result<Green, LineageRefused> {
    self
      .seats
      .get_mut(handle.seat)
      .filter(|place| place.generation == handle.generation)
      .and_then(|place| place.green.take())
      .ok_or(LineageRefused::Stale)
  }

  /// `handle`'s engine out of its seat with the lineage's arenas in it, to be put back by [`Lineage::put_back`]: what a
  /// replay that runs over many slices holds (a recovery's), and what [`Lent`] holds for the length of a borrow. Refused
  /// `Busy` while another member is out.
  pub fn take(&mut self, handle: LineageGreen) -> Result<Green, LineageRefused> {
    if self.out.is_some() {
      return Err(LineageRefused::Busy);
    }
    let mut green = self.unseat(handle)?;
    green.swap_arenas(&mut self.arenas);
    self.out = Some(handle);
    Ok(green)
  }

  /// Puts back the engine [`Lineage::take`] gave out for `handle`, the arenas with it. Refused `Stale` for a handle that
  /// is not the one out, the green then dropped with what it holds.
  pub fn put_back(&mut self, handle: LineageGreen, green: Green) -> Result<(), LineageRefused> {
    if self.out != Some(handle) {
      return Err(LineageRefused::Stale);
    }
    self.restore(handle, green);
    Ok(())
  }

  /// [`Lineage::put_back`] for the member known to be out.
  fn restore(&mut self, handle: LineageGreen, mut green: Green) {
    green.swap_arenas(&mut self.arenas);
    if let Some(place) = self.seats.get_mut(handle.seat) {
      place.green = Some(green);
    }
    self.out = None;
  }

  /// Runs `call` on `handle`'s green with the lineage's arenas lent to it.
  pub fn with<R>(
    &mut self,
    handle: LineageGreen,
    call: impl FnOnce(&mut Green) -> R,
  ) -> Result<R, LineageRefused> {
    let mut lent = Lent::new(&mut *self, handle)?;
    Ok(call(&mut lent))
  }

  /// A green whose version 0 is `from`'s `version`, made without a walk (module docs): what it costs is what changed
  /// after `version`.
  pub fn branch(
    &mut self,
    from: LineageGreen,
    version: u64,
  ) -> Result<LineageGreen, LineageRefused> {
    let (branch, reverted) = self.with(from, |green| green.branch_at(version))??;
    self.reverted = self.reverted.saturating_add(reverted);
    Ok(self.seat(branch))
  }

  /// Retires `handle`'s green: its seat is freed and its handle stale from here on, and what it held is returned as a
  /// [`Release`], whose steps give its holds on the shared nodes back ([`Lineage::release_step`]). Refused `Busy` while a
  /// member is out.
  pub fn retire(&mut self, handle: LineageGreen) -> Result<Release, LineageRefused> {
    if self.out.is_some() {
      return Err(LineageRefused::Busy);
    }
    let mut green = self.unseat(handle)?;
    let maps = green.releasing();
    if let Some(place) = self.seats.get_mut(handle.seat) {
      place.generation = place.generation.saturating_add(1);
    }
    self.free.push(handle.seat);
    Ok(Release { maps })
  }

  /// One step of `release`: one node's hold given back, the node freed, and its file's block let go, when no other
  /// member names it. False once every hold is back; refused `Busy` while a member is out, since the arenas are then in
  /// its engine.
  pub fn release_step(&mut self, release: &mut Release) -> Result<bool, LineageRefused> {
    if self.out.is_some() {
      return Err(LineageRefused::Busy);
    }
    Ok(self.arenas.release_step(&mut release.maps))
  }

  /// The members seated.
  pub fn len(&self) -> usize {
    self
      .seats
      .iter()
      .filter(|place| place.green.is_some())
      .count()
      .saturating_add(usize::from(self.out.is_some()))
  }

  /// Whether no member is seated or out.
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// Keys every branch set back: what a branch at an older version cost beyond its counts.
  pub fn reverted(&self) -> u64 {
    self.reverted
  }

  /// Nodes any walk of the lineage's maps visited: a branch moves it by nothing (module docs).
  pub fn walked(&self) -> u64 {
    self.arenas.visited()
  }

  /// Nodes copied because two greens shared them.
  pub fn copied(&self) -> u64 {
    self.arenas.copied()
  }

  /// Nodes alive in the lineage's arenas.
  pub fn live_nodes(&self) -> usize {
    self.arenas.live()
  }

  /// Links that named no node: zero in every correct use.
  pub fn missing(&self) -> u64 {
    self.arenas.missing()
  }
}

#[cfg(test)]
mod tests;
