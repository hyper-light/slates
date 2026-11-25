//! A green's named blocks' holds on the shard's content store (A-760 (b) 4d; main's order of 2026-11-23).
//!
//! **What.** A file a green holds by its content-plane object's name alone holds no bytes: the object's are the content
//! store's, read through the plane by whatever reads the file. So the store must hold the object while a block names
//! it: each named block takes one hold of its object as it is made and gives it back as it is freed
//! (`slates_merge::engine::NameHold`). Every free decision of the store's texts goes through [`freeing`] (main's
//! ruling of 2026-11-23: the invariant sits at the free decision), which applies the greens' pending holds, the takes
//! before the gives, before it chooses what to free: so the landing's own hold of a text given back
//! (`svn_commit::release_texts`) never frees one the green named in the same turn, and a recovery's sweep of unheld
//! windows (`vcs_store::release_unheld_windows`) never frees one a replayed green names. An object one block lets go and
//! another names in the same drain is never freed between.
//!
//! **Engines let go.** The owner gives a tracked engine's holds back before it drops it (`Greens`, which keeps them
//! owed until the next free decision): a tracked engine dropped with a hold live is a bug, counted
//! ([`NAME_HOLD_DROPPED_LIVE`], from `slates_merge::engine::dropped_with_name_holds`).
//!
//! **Restarts.** The store's holders reset at a restart (`ContentStore::restore_nodes`: held by no one until a holder
//! puts or holds its object again), and an engine is tracked from its first drain after it is installed, when it takes
//! one hold for each named block it holds (`Green::track_name_holds`): a replayed green's holds are taken once, never on
//! top of the ones before the restart.

use slates_merge::engine::NameHold;
use slates_vcs::{ContentId, ContentStore};

/// Counted for each hold a green took of an object the content store does not hold, or with no content store: zero in
/// every correct use, since a named file's object is in the store before any version names it (A-761).
/// Format: a counter name in the daemon's status report.
pub(crate) const NAME_HOLD_UNHELD: &str = "merge.name_hold.unheld";

/// Counted for each tracked engine dropped with a hold still live (its owner did not give them back first): a leaked
/// hold's bug signal, zero in every correct use. Format: a counter name in the daemon's status report.
pub(crate) const NAME_HOLD_DROPPED_LIVE: &str = "merge.name_hold.dropped_live";

/// The process's count of tracked engines dropped live already counted by some shard ([`NAME_HOLD_DROPPED_LIVE`]).
static DROPPED_COUNTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A free decision of the content store's texts (module docs): the greens' pending holds applied, the takes first,
/// then `free` run over the store (none without one), within the shard's budget.
pub(crate) fn freeing<T>(
  state: &mut crate::state::ShardState,
  free: impl FnOnce(Option<&mut ContentStore>) -> T,
) -> T {
  let holds = state.greens.take_name_holds();
  let (unheld, answer) = super::within_budget(state, 0, |stores| {
    let unheld = apply(stores.content_mut(), holds);
    (unheld, free(stores.content_mut()))
  });
  if unheld > 0 {
    state.count(NAME_HOLD_UNHELD, unheld);
  }
  let dropped = slates_merge::engine::dropped_with_name_holds();
  let before = DROPPED_COUNTED.swap(dropped, std::sync::atomic::Ordering::Relaxed);
  if dropped > before {
    state.count(NAME_HOLD_DROPPED_LIVE, dropped.saturating_sub(before));
  }
  answer
}

/// Applies `holds` to `store`, the takes first: the takes refused (no such object, or no store).
fn apply(store: Option<&mut ContentStore>, holds: Vec<NameHold>) -> u64 {
  if holds.is_empty() {
    return 0;
  }
  let takes = holds
    .iter()
    .filter(|hold| matches!(hold, NameHold::Take(_)))
    .count() as u64;
  let Some(store) = store else {
    return takes;
  };
  let mut unheld = 0u64;
  for hold in &holds {
    if let NameHold::Take(identity) = hold
      && store.hold(&ContentId(identity.hash)).is_err()
    {
      unheld = unheld.saturating_add(1);
    }
  }
  for hold in &holds {
    if let NameHold::Give(identity) = hold {
      let _ = store.release(&ContentId(identity.hash));
    }
  }
  unheld
}

/// A named text a green's verdict or replay reads that the content plane cannot give now (A-760 (b) 4d): let go to the
/// node's store (`Away`, which its async caller brings back and retries on), or held by no one here (`Missing`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unlendable {
  /// Its windows were let go ([`ContentStore::is_away`]).
  Away(slates_merge::named::Identity),
  /// The content store does not hold it, or there is no content store.
  Missing(slates_merge::named::Identity),
}

/// Counted for each verdict judged again after its green was lent the named texts it read unlent (A-760 (b) 4d): the
/// non-vacuity counter of a base read through the plane. Format: a counter name in the daemon's status report.
pub(crate) const NAME_LENT_VERDICTS: &str = "merge.name_lend.verdicts";

/// Counted for each verdict that read a named text unlent again once lent every one its first judge named: a bug
/// signal, zero in every correct use (`Green::unlent_reads` names them all). Format: a counter name in the daemon's
/// status report.
pub(crate) const NAME_LEND_MISSED: &str = "merge.name_lend.missed";

/// Where a verdict, a commit or a replay reads the named texts it is lent (A-887): its green, the paths its increment
/// names (a copy's source among them) and the versions it reads them at. A text whose windows were let go is noted
/// with the path and version it is the green's file at, found among these, by which a Mercurial green's text comes back
/// (`hg_named`); found on the miss alone, a cold path.
pub(crate) struct ReadIn<'a> {
  /// The green.
  pub(crate) green: slates_db::catalog::VolumeId,
  /// The paths read.
  pub(crate) paths: Vec<&'a str>,
  /// The versions they are read at: the head, and the base the work was made over.
  pub(crate) versions: [u64; 2],
}

impl<'a> ReadIn<'a> {
  /// A work's verdict on `green`, over `inc`: the paths its operations name, at the green's head and the work's base.
  pub(crate) fn verdict(
    green: slates_db::catalog::VolumeId,
    head: u64,
    inc: &'a slates_merge::engine::Increment,
  ) -> ReadIn<'a> {
    ReadIn {
      green,
      paths: inc.doc.paths.paths().iter().map(String::as_str).collect(),
      versions: [head, inc.base],
    }
  }

  /// A commit or a replay on `green` of `applied`, read at `versions`: the paths each change names, a copy's source too.
  pub(crate) fn applied(
    green: slates_db::catalog::VolumeId,
    versions: [u64; 2],
    applied: &'a [slates_merge::chain_entry::Applied],
  ) -> ReadIn<'a> {
    use slates_merge::chain_entry::Applied;
    let mut paths = Vec::with_capacity(applied.len());
    for change in applied {
      match change {
        Applied::Spliced(path, _) | Applied::Created(path, _) => paths.push(path.as_str()),
        Applied::Copied { dst, from, .. } => paths.extend([dst.as_str(), from.as_str()]),
        _ => {}
      }
    }
    ReadIn {
      green,
      paths,
      versions,
    }
  }

  /// The path and version at which the green holds `identity` as a file, among these.
  fn site_of(
    &self,
    state: &crate::state::ShardState,
    identity: slates_merge::named::Identity,
  ) -> Option<(slates_db::catalog::VolumeId, &'a str, u64)> {
    let engine = state.greens.get(&self.green)?;
    self.versions.iter().find_map(|version| {
      self.paths.iter().find_map(|path| {
        matches!(
          engine.content_ref_at(path, *version),
          Some(slates_merge::engine::ContentAt::Named(named)) if named == identity
        )
        .then_some((self.green, *path, *version))
      })
    })
  }
}

/// Lends the engine of `read_in`'s green each of `wanted`, read whole from the shard's content plane (A-760 (b) 4d):
/// refused, nothing lent, naming the first the plane cannot give now.
pub(crate) fn lend_from_plane(
  state: &mut crate::state::ShardState,
  read_in: &ReadIn<'_>,
  wanted: &std::collections::BTreeSet<slates_merge::named::Identity>,
) -> Result<(), Unlendable> {
  let green = read_in.green;
  let read = read_from_plane(state, wanted, Some(read_in))?;
  let Some(mut engine) = state.greens.get_mut(&green) else {
    return match wanted.first() {
      Some(identity) => Err(Unlendable::Missing(*identity)),
      None => Ok(()),
    };
  };
  for (identity, bytes) in read {
    engine.lend(identity, bytes);
  }
  Ok(())
}

/// Each of `wanted` read whole from the shard's content plane (A-760 (b) 4d), to lend an engine: refused naming the
/// first the plane cannot give now, a let-go one noted where `read_in` reads it ([`ReadIn`]).
pub(crate) fn read_from_plane(
  state: &crate::state::ShardState,
  wanted: &std::collections::BTreeSet<slates_merge::named::Identity>,
  read_in: Option<&ReadIn<'_>>,
) -> Result<Vec<(slates_merge::named::Identity, Vec<u8>)>, Unlendable> {
  use slates_merge::named::Objects;
  let mut read = Vec::with_capacity(wanted.len());
  {
    let Some(store) = state.vcs_stores.content() else {
      return match wanted.first() {
        Some(identity) => Err(Unlendable::Missing(*identity)),
        None => Ok(read),
      };
    };
    let objects = super::named_objects::PlaneObjects {
      store,
      kept_in: &state.store.content,
    };
    for identity in wanted {
      let refused = || {
        if store.is_away(&ContentId(identity.hash)) {
          Unlendable::Away(*identity)
        } else {
          Unlendable::Missing(*identity)
        }
      };
      let len = usize::try_from(identity.len).map_err(|_| refused())?;
      let mut bytes = Vec::new();
      bytes.try_reserve_exact(len).map_err(|_| refused())?;
      bytes.resize(len, 0);
      if len > 0 && objects.read(identity, 0, &mut bytes).is_err() {
        // Its windows let go: noted for the call's async frame, which parks it (`named_objects::away_scope`).
        let seen = read_in.and_then(|read_in| read_in.site_of(state, *identity));
        super::named_objects::note_if_away(&objects, *identity, (0, identity.len), seen);
        return Err(refused());
      }
      read.push((*identity, bytes));
    }
  }
  Ok(read)
}

/// Lets go every named text green `green`'s engine was lent (A-760 (b) 4d), so no named block holds bytes between
/// steps.
pub(crate) fn let_go_lent(
  state: &mut crate::state::ShardState,
  green: slates_db::catalog::VolumeId,
) {
  if let Some(mut engine) = state.greens.get_mut(&green) {
    engine.let_named_bytes_go();
  }
}

/// Commits green `green`'s held verdict `judged` (`Green::commit_judged`), the named texts its commit reads lent it from
/// the content plane first and let go after (A-760 (b) 4d): `None` for a green not here, or a commit refused (its head
/// moved, or a text the plane cannot give now).
pub(crate) fn commit_judged_lent(
  state: &mut crate::state::ShardState,
  green: slates_db::catalog::VolumeId,
  judged: slates_merge::engine::Judged,
) -> Option<(u64, Vec<slates_merge::chain_entry::Applied>)> {
  let unlent = state.greens.get(&green)?.commit_unlent(&judged);
  let at = judged.at();
  if !unlent.is_empty()
    && lend_from_plane(
      state,
      &ReadIn::applied(green, [at, at], judged.applied()),
      &unlent,
    )
    .is_err()
  {
    return None;
  }
  let mut engine = state.greens.get_mut(&green)?;
  let committed = engine.commit_judged(judged).ok();
  engine.let_named_bytes_go();
  committed
}

/// Applies `applied` to green `green` for `inc` as its chain entry records it (`Green::apply_recorded`), the named texts
/// its apply reads lent it from the content plane first and let go after (A-760 (b) 4d): the version it committed, or
/// `None` for a green not here or a text the plane cannot give now.
pub(crate) fn apply_recorded_lent(
  state: &mut crate::state::ShardState,
  green: slates_db::catalog::VolumeId,
  (inc, applied): (
    &mut slates_merge::engine::Increment,
    &[slates_merge::chain_entry::Applied],
  ),
) -> Option<u64> {
  let engine = state.greens.get(&green)?;
  let (unlent, head) = (engine.recorded_unlent(inc, applied), engine.head());
  drop(engine);
  if !unlent.is_empty()
    && lend_from_plane(
      state,
      &ReadIn::applied(green, [head, inc.base], applied),
      &unlent,
    )
    .is_err()
  {
    return None;
  }
  let mut engine = state.greens.get_mut(&green)?;
  let version = engine.apply_recorded(inc, applied);
  engine.let_named_bytes_go();
  Some(version)
}

/// Counted: the bytes of committed texts' windows a landing let go once its changeset was durable on the node's store
/// (A-760 (b) 4d, main's ruling (B) of 2026-11-23: a landed text is repository, kept on disk; RAM holds nothing of it).
/// Format: a counter name in the daemon's status report.
pub(crate) const LANDED_TEXT_LET_GO_BYTES: &str = "merge.name_hold.landed_let_go_bytes";

/// Counted: a landed text's window a let-go kept because a reader that brought it back reads it still
/// (`vcs_store::landed_pin`'s `"reader"`). Format: a counter name in the daemon's status report.
pub(crate) const LANDED_TEXT_PINNED_READER: &str = "merge.name_hold.landed_pinned_reader";

/// Counted: a landed text's window a let-go kept because a home holds it for its region here (`vcs_store::landed_pin`'s
/// `"homes"`). Format: a counter name in the daemon's status report.
pub(crate) const LANDED_TEXT_PINNED_HOMES: &str = "merge.name_hold.landed_pinned_homes";

/// Unpins each of `texts`, an import's texts pinned as it sealed them (`svn_import::texts`, `ContentStore::pin_unlanded`;
/// main's ruling (B) of 2026-11-24), once each: their batch landed, or their write ended. Called before the holds they
/// came with are given back, as the store asks (a freed window takes its count with it).
pub(crate) fn unpin_texts(state: &mut crate::state::ShardState, texts: &[[u8; 32]]) {
  if texts.is_empty() {
    return;
  }
  super::within_budget(state, 0, |stores| {
    if let Some(content) = stores.content_mut() {
      for text in texts {
        content.unpin_unlanded(&ContentId(*text));
      }
    }
  });
}

/// Lets go the windows of each of `texts`, a landing's committed texts, once its changeset is durable on the node's
/// store (main's ruling (B)): each text's entry and its holds stay (the green's, by name), its windows go `Away`, and the
/// first read brings them back from the node's store. Pinned as any landed window is (`vcs_store::landed_pin`). The
/// bytes let go, counted. A synchronous test history, which can bring nothing back, keeps them
/// (`vcs_store::KEEP_LANDED`).
pub(crate) fn let_go_landed_texts(state: &mut crate::state::ShardState, texts: &[[u8; 32]]) -> u64 {
  #[cfg(test)]
  if super::KEEP_LANDED.with(core::cell::Cell::get) {
    return 0;
  }
  if texts.is_empty() {
    return 0;
  }
  // Each window kept counted by the holder that pinned it, so a text left resident says why.
  let (reader, homes) = (core::cell::Cell::new(0u64), core::cell::Cell::new(0u64));
  let pinned = |window: &ContentId| {
    let by = super::landed_pin(window);
    match by {
      Some("reader") => reader.set(reader.get().saturating_add(1)),
      Some(_) => homes.set(homes.get().saturating_add(1)),
      None => {}
    }
    by
  };
  let freed = super::within_budget(state, 0, |stores| {
    let Some(content) = stores.content_mut() else {
      return 0;
    };
    let mut freed = 0u64;
    for text in texts.iter().map(|text| ContentId(*text)) {
      if content.contains(&text) {
        let (bytes, _) = content.let_go_object_landed(&text, &pinned);
        freed = freed.saturating_add(bytes);
      }
    }
    freed
  });
  if freed > 0 {
    state.count(LANDED_TEXT_LET_GO_BYTES, freed);
  }
  state.count(LANDED_TEXT_PINNED_READER, reader.get());
  state.count(LANDED_TEXT_PINNED_HOMES, homes.get());
  freed
}

/// Gives back the holds a bring-back took of `texts` (`svn_texts::bring_whole`'s, landed texts brought whole for a step)
/// and lets their windows go again (main's ruling (B): a landed text's bytes are the node's store's, so RAM keeps none of
/// them once the step that wanted them whole has run; a green's hold keeps their entries).
pub(crate) fn give_back_brought(state: &mut crate::state::ShardState, texts: &[[u8; 32]]) {
  if texts.is_empty() {
    return;
  }
  crate::repository::svn_commit::release_texts(state, None, texts);
  let_go_landed_texts(state, texts);
}

/// Whether `store` keeps any byte of the object `identity` (`len` bytes) resident: a window of it not let go (main's
/// ruling (B): a landed text keeps its entry, the green's hold, and no window once its changeset is durable).
pub(crate) fn resident(store: &ContentStore, identity: [u8; 32], len: u64) -> bool {
  let root = ContentId(identity);
  let mut at = 0u64;
  while at < len.max(1) {
    let Some((window, start, width)) = store.window_holding(&root, len, at) else {
      return false;
    };
    // Held and not let go: `window_holding` names an object of one window by its root whether or not the store holds
    // it, so an object the store holds nothing of read as resident (the crash test's `kept` and the copy-back red read
    // it so).
    if store.contains(&window) && !store.is_away(&window) {
      return true;
    }
    at = start.saturating_add(width.max(1));
  }
  false
}
