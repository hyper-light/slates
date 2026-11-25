//! A green's named content read from the shard's content plane (A-760; §4.17.1 "A push's intake"; A-761: Subversion's
//! intake writes each text once into the plane, durable before its revision's entry).
//!
//! **What.** [`PlaneObjects`] is the engine's [`Objects`] over vcs_store's content store: an object found by its
//! identity's hash (`ContentStore::root_of`: the node-local root it is kept under), and read straight into the run's
//! buffer a window's range at a time (`ContentStore::window_range_in`), so a read copies its own bytes and nothing more.
//! A recovery's replay holds the shard read-only, so its chunk store, which a step lends the content store, is given in
//! its place (`kept_in`: the windows are read from it where the store holds none lent).

use slates_merge::named::{Identity, ObjectMissing, Objects};
use slates_vcs::{ContentId, ContentStore};

use super::hg_named::ReadAt;

/// Where a reader read a named file, as it knew it: the green, the path there and the version (a Mercurial green's
/// named file is brought back by them, `hg_named`). None for a reader that reads an object by its identity alone.
pub(crate) type Seen<'p> = Option<(slates_db::catalog::VolumeId, &'p str, u64)>;

/// [`Seen`] kept with a read that met a let-go window.
fn read_at(seen: Seen<'_>) -> Option<ReadAt> {
  seen.map(|(green, path, version)| ReadAt {
    green,
    version,
    path: path.to_owned(),
  })
}

/// The engine's named objects over the content plane (module docs).
#[derive(Clone, Copy)]
pub(crate) struct PlaneObjects<'a> {
  /// The content store.
  pub(crate) store: &'a ContentStore,
  /// What its windows are read from where none is lent to it now: the shard's chunk store, or nothing (`&()`).
  pub(crate) kept_in: &'a dyn std::any::Any,
}

/// A file a green holds by its content-plane object alone, met by a reader that reads no plane (A-760 (b) 4b, main
/// 01a20182's ruling of 2026-11-23): the file's path and the object. Only a Subversion green holds a file so, and only
/// once piece 4d drops a named block's bytes; until then no green answers one, so a reader's refusal of it is never
/// met. Piece 4d gives the readers that serve a Subversion green the plane's read, a sync reader the typed refusal its
/// async caller brings the object back on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamedUnread {
  /// The file.
  pub(crate) path: String,
  /// The object it is held by.
  pub(crate) identity: Identity,
}

impl std::fmt::Display for NamedUnread {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "{} is held by its content-plane object ({} bytes) alone, which this reader does not read",
      self.path, self.identity.len
    )
  }
}

/// A green's file content at a version (`slates_merge::engine::ContentAt`, or a current file's `Content`) as the bytes
/// a reader with no plane takes, or the typed refusal naming the file and its object ([`NamedUnread`]).
pub(crate) fn bytes_of<'a>(
  path: &str,
  content: impl Into<slates_merge::engine::ContentAt<'a>>,
) -> Result<std::borrow::Cow<'a, [u8]>, NamedUnread> {
  content.into().into_bytes().map_err(|identity| NamedUnread {
    path: path.to_owned(),
    identity,
  })
}

/// [`bytes_of`] for a current file (`slates_merge::engine::Content`), its bytes lent.
pub(crate) fn lent_bytes_of<'a>(
  path: &str,
  content: slates_merge::engine::Content<'a>,
) -> Result<&'a [u8], NamedUnread> {
  content.bytes().map_err(|identity| NamedUnread {
    path: path.to_owned(),
    identity,
  })
}

/// The content plane over `stores`' content store, its windows read from `kept_in` where none is lent to it (the
/// shard's chunk store, or `&()` where the chunks are lent for the request: `vcs_store::lent_state`).
pub(crate) fn plane<'a>(
  stores: &'a super::VcsStores,
  kept_in: &'a dyn std::any::Any,
) -> Option<PlaneObjects<'a>> {
  stores
    .content()
    .map(|store| PlaneObjects { store, kept_in })
}

/// `content` (a green's file, or a text it holds) whole: its bytes, or its object's read through `plane` where the green
/// holds it by name alone (A-760 (b) 4d); none where the plane cannot give it now.
pub(crate) fn whole_of(
  content: slates_merge::engine::ContentAt<'_>,
  plane: Option<&PlaneObjects<'_>>,
  seen: Seen<'_>,
) -> Option<Vec<u8>> {
  match content {
    slates_merge::engine::ContentAt::Bytes(bytes) => Some(bytes.into_owned()),
    slates_merge::engine::ContentAt::Named(identity) => {
      let plane = plane?;
      let mut bytes = vec![0u8; usize::try_from(identity.len).ok()?];
      if !bytes.is_empty() && plane.read(&identity, 0, &mut bytes).is_err() {
        note_if_away(plane, identity, (0, identity.len), seen);
        return None;
      }
      Some(bytes)
    }
  }
}

/// The bytes of a green's file page (`Green::content_page_at`'s, at `offset` and at most `want` long): the page's own,
/// or its object's range read through `plane` where the green holds the file by name alone (A-760 (b) 4d). Refused
/// naming the file and its object where the plane cannot give them now.
pub(crate) fn page_of<'a>(
  (path, green_at): (&str, Option<(slates_db::catalog::VolumeId, u64)>),
  page: slates_merge::engine::ContentAt<'a>,
  (offset, want): (u64, u64),
  plane: Option<&PlaneObjects<'_>>,
) -> Result<std::borrow::Cow<'a, [u8]>, NamedUnread> {
  let identity = match page {
    slates_merge::engine::ContentAt::Bytes(bytes) => return Ok(bytes),
    slates_merge::engine::ContentAt::Named(identity) => identity,
  };
  let unread = || NamedUnread {
    path: path.to_owned(),
    identity,
  };
  let take = identity.len.saturating_sub(offset).min(want);
  let mut bytes = vec![0u8; usize::try_from(take).map_err(|_| unread())?];
  if !bytes.is_empty() {
    let plane = plane.ok_or_else(unread)?;
    if plane.read(&identity, offset, &mut bytes).is_err() {
      let seen = green_at.map(|(green, version)| (green, path, version));
      note_if_away(plane, identity, (offset, take), seen);
      return Err(unread());
    }
  }
  Ok(std::borrow::Cow::Owned(bytes))
}

/// Green `green`'s current file `path` whole, read through the shard's content plane where it is held by name
/// ([`whole_of`]).
pub(crate) fn read_file(
  state: &crate::state::ShardState,
  green: slates_db::catalog::VolumeId,
  path: &str,
) -> Option<Vec<u8>> {
  let engine = state.greens.get(&green)?;
  let content = engine.content(path)?;
  whole_of(
    content.into(),
    plane(&state.vcs_stores, &state.store.content).as_ref(),
    Some((green, path, engine.head())),
  )
}

/// The text green `green` holds under `identity` (A-716's table) whole, read through the shard's content plane where it
/// is held by name ([`whole_of`]).
pub(crate) fn read_held(
  state: &crate::state::ShardState,
  green: slates_db::catalog::VolumeId,
  identity: &slates_merge::held::Identity,
) -> Option<Vec<u8>> {
  let engine = state.greens.get(&green)?;
  let content = engine.held(identity)?;
  whole_of(
    content.into(),
    plane(&state.vcs_stores, &state.store.content).as_ref(),
    None,
  )
}

// The windows readers brought back and read still, on the shard's thread (a reader's task runs there): kept from a
// landed let-go until each reader's last step (`vcs_store::landed_pin`), each a count of its readers.
thread_local! {
  static READ_PINS: std::cell::RefCell<std::collections::BTreeMap<ContentId, u32>> =
    const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// Whether a reader keeps `window` until its read's last step ([`ReadPin`]).
pub(crate) fn read_pinned(window: &ContentId) -> bool {
  READ_PINS.with(|pins| pins.borrow().contains_key(window))
}

/// The windows of a range of an object a reader brought back ([`bring_back_range`]), kept from a landed let-go until it
/// is dropped: at the read's last step, answered or cancelled, so a reader that goes away keeps nothing.
#[must_use = "the windows are kept only while the pin lives"]
pub(crate) struct ReadPin {
  windows: Vec<ContentId>,
}

impl ReadPin {
  /// `windows` kept.
  fn of(windows: Vec<ContentId>) -> ReadPin {
    READ_PINS.with(|pins| {
      let mut pins = pins.borrow_mut();
      for window in &windows {
        let count = pins.entry(*window).or_insert(0);
        *count = count.saturating_add(1);
      }
    });
    ReadPin { windows }
  }
}

impl Drop for ReadPin {
  fn drop(&mut self) {
    // A pin a task still held as its thread ends finds the table gone with it: nothing left to keep.
    let _ = READ_PINS.try_with(|pins| {
      let mut pins = pins.borrow_mut();
      for window in &self.windows {
        if let Some(count) = pins.get_mut(window) {
          *count = count.saturating_sub(1);
          if *count == 0 {
            pins.remove(window);
          }
        }
      }
    });
  }
}

/// Counted: a range of a named text a reader brought back from the node's store before it read (A-760 (b) 4d, main's
/// (B): a landed text's windows go once durable, so a fresh commit's first read is one). Format: a counter name.
pub(crate) const READ_BROUGHT_BACK: &str = "merge.name_read.brought_back";
/// Counted: the nanoseconds those readers waited for it. Format: a counter name.
pub(crate) const READ_BROUGHT_BACK_WAIT_NS: &str = "merge.name_read.brought_back_wait_ns";

/// Brings back from the node's store every let-go window of `identity` covering `[offset, offset + len)` (at least the
/// one holding `offset`), and keeps them until the returned pin drops (A-760 (b) 4d; main's ruling: the windows the
/// parked read's range needs, held until its terminal step): so the read served again after it meets none let go, by
/// construction. Each window is pinned before the first wait, so none goes while another comes back. Counted, with the
/// wait. Refused as `read_through::bring_back` refuses.
pub(crate) async fn bring_back_range(
  identity: Identity,
  (offset, len): (u64, u64),
) -> Result<ReadPin, super::read_through::ReadThroughRefused> {
  let began = slates_rt::futures::now_ns();
  let root = ContentId(identity.hash);
  let (pin, windows) = pin_range(identity, (offset, len))?;
  // A Subversion text is its repository's on the node's store (read by repository and identity); any other object is
  // its landed changeset's Payload, read by identity alone (`read_through`).
  let repository =
    crate::state::with_state(|state| state.vcs_stores.subversion_storing(&identity.hash)).flatten();
  for (window, start) in &windows {
    match repository {
      Some(repository) => bring_back_text_window(repository, identity, (*window, *start)).await?,
      None => {
        let waited = slates_git::store::packed::Waited {
          pack: root,
          len: identity.len,
          offset: *start,
        };
        super::read_through::bring_back(waited).await?;
      }
    }
  }
  count_brought_back(1, began);
  Ok(pin)
}

/// Counts `reads` brought back ([`READ_BROUGHT_BACK`]) and the wait since `began`.
fn count_brought_back(reads: u64, began: u64) {
  let waited_ns = slates_rt::futures::now_ns().saturating_sub(began);
  let _ = crate::state::with_state(|state| {
    state.count(READ_BROUGHT_BACK, reads);
    state.count(READ_BROUGHT_BACK_WAIT_NS, waited_ns);
  });
}

/// The windows of `identity` covering `[offset, offset + len)` (at least the one holding `offset`), each with where it
/// begins, kept from a landed let-go until the pin drops: pinned before any wait, so none goes while another comes
/// back.
pub(crate) fn pin_range(
  identity: Identity,
  (offset, len): (u64, u64),
) -> Result<(ReadPin, Vec<(ContentId, u64)>), super::read_through::ReadThroughRefused> {
  use super::read_through::ReadThroughRefused;
  let root = ContentId(identity.hash);
  let windows = crate::state::with_state(|state| {
    let content = state.vcs_stores.content()?;
    let end = offset.saturating_add(len.max(1)).min(identity.len.max(1));
    let mut windows = Vec::new();
    let mut at = offset;
    while at < end {
      let (window, start, width) = content.window_holding(&root, identity.len, at)?;
      windows.push((window, start));
      at = start.saturating_add(width.max(1));
    }
    Some(windows)
  })
  .flatten()
  .ok_or(ReadThroughRefused::NoStore)?;
  let pin = ReadPin::of(windows.iter().map(|(window, _)| *window).collect());
  Ok((pin, windows))
}

/// A read that met a window of a named text let go (A-760 (b) 4d; main's ruling (B): a landed text's windows go once
/// durable): the object and the range the read wanted, which its async caller brings back ([`bring_back_reads`]) and
/// serves the read again over, never answering the miss to its client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AwayRead {
  /// The object.
  pub(crate) identity: Identity,
  /// Where the read began in it.
  pub(crate) offset: u64,
  /// How many bytes it wanted.
  pub(crate) len: u64,
  /// Where the reader read it, where it knew ([`Seen`]): a Mercurial green's named file is brought back by it alone
  /// (A-887, `hg_named`).
  pub(crate) at: Option<ReadAt>,
}

// The reads that met a let-go window in the call running now on this shard's thread ([`away_scope`]): a sync call runs
// to its end with no other between, so each call's are its own.
thread_local! {
  static AWAY_READS: std::cell::RefCell<Option<Vec<AwayRead>>> = const { std::cell::RefCell::new(None) };
}

/// Notes the read of `identity` over `(offset, len)` that `plane` refused as one that met a let-go window
/// ([`note_away`]), where a window of that range was let go: a refusal for any other cause (the object not here) is
/// the reader's own.
pub(crate) fn note_if_away(
  plane: &PlaneObjects<'_>,
  identity: Identity,
  (offset, len): (u64, u64),
  seen: Seen<'_>,
) {
  let root = ContentId(identity.hash);
  let end = offset.saturating_add(len.max(1)).min(identity.len.max(1));
  let mut at = offset;
  while at < end {
    let Some((window, start, width)) = plane.store.window_holding(&root, identity.len, at) else {
      return;
    };
    if plane.store.is_away(&window) {
      note_away(AwayRead {
        identity,
        offset,
        len,
        at: read_at(seen),
      });
      return;
    }
    at = start.saturating_add(width.max(1));
  }
}

/// Notes `read` as one that met a let-go window, inside an [`away_scope`]; nothing outside one (a reader outside a call
/// that parks answers its refusal).
pub(crate) fn note_away(read: AwayRead) {
  AWAY_READS.with(|reads| {
    if let Some(reads) = reads.borrow_mut().as_mut() {
      reads.push(read);
    }
  });
}

/// How many reads the call running now has noted so far ([`away_scope`]); none outside one. A caller that asks after
/// each of its steps (a virtio-fs device after each chain, `crate::virtiofs`) tells the step that met a let-go window.
/// Unix alone, as its only caller is.
#[cfg(unix)]
pub(crate) fn noted_in_scope() -> usize {
  AWAY_READS.with(|reads| reads.borrow().as_ref().map_or(0, Vec::len))
}

/// Puts the caller's notes back however the scope ends.
struct AwayRestore(Option<Option<Vec<AwayRead>>>);

impl Drop for AwayRestore {
  fn drop(&mut self) {
    if let Some(outer) = self.0.take() {
      let _ = AWAY_READS.try_with(|reads| {
        let _ = reads.replace(outer);
      });
    }
  }
}

/// `f`'s answer and the reads in it that met a let-go window ([`note_away`]); an enclosing scope notes them too.
pub(crate) fn away_scope<T>(f: impl FnOnce() -> T) -> (T, Vec<AwayRead>) {
  let outer = AWAY_READS.with(|reads| reads.replace(Some(Vec::new())));
  let mut restore = AwayRestore(Some(outer));
  let answer = f();
  let mine = AWAY_READS
    .with(|reads| reads.replace(restore.0.take().unwrap_or_default()))
    .unwrap_or_default();
  for read in &mine {
    note_away(read.clone());
  }
  (answer, mine)
}

/// The calls a bridge parked on reads that met a let-go window (A-760 (b) 4d; main's ruling: one entry per parked
/// request, bounded by the bridge's in-flight bound, a typed refusal at the bound), each with the reads it waits on.
#[derive(Debug, Default)]
pub(crate) struct ParkedReads {
  parked: std::collections::BTreeMap<u64, Vec<AwayRead>>,
  next: u64,
}

impl ParkedReads {
  /// Parks a call waiting on `reads`: its entry, or `None` at `bound` entries (the caller answers its refusal).
  pub(crate) fn park(&mut self, reads: Vec<AwayRead>, bound: usize) -> Option<u64> {
    if self.parked.len() >= bound {
      return None;
    }
    let id = self.next;
    self.next = self.next.wrapping_add(1);
    self.parked.insert(id, reads);
    Some(id)
  }

  /// The call `id` ended (answered or cancelled): its entry gone.
  pub(crate) fn release(&mut self, id: u64) {
    self.parked.remove(&id);
  }
}

/// Brings back every window `reads` met let go, each read's range, and keeps them all until the pins drop: what a parked
/// call holds while it is served again. A Mercurial green's named file comes back by the revision its green's changeset
/// names where it was read (A-887, `hg_named`), each green's and version's reads through one read of its manifest;
/// any other object by its range ([`bring_back_range`]). Refused as the first refused bring-back.
pub(crate) async fn bring_back_reads(
  reads: &[AwayRead],
) -> Result<Vec<ReadPin>, super::read_through::ReadThroughRefused> {
  let began = slates_rt::futures::now_ns();
  let mut pins = Vec::with_capacity(reads.len());
  // A Mercurial green's reads, by its repository, the green and the version read: each the paths and objects wanted.
  type Wanted = Vec<(Identity, String)>;
  let mut mercurial: Vec<((slates_db::catalog::RepositoryId, ReadAt), Wanted)> = Vec::new();
  for read in reads {
    let repository = read.at.as_ref().and_then(|at| {
      crate::state::with_state(|state| super::hg_named::repository_of(state, at.green)).flatten()
    });
    let (Some(repository), Some(at)) = (repository, read.at.as_ref()) else {
      pins.push(bring_back_range(read.identity, (read.offset, read.len)).await?);
      continue;
    };
    pins.push(pin_range(read.identity, (read.offset, read.len))?.0);
    let wanted = (read.identity, at.path.clone());
    let key = ReadAt {
      path: String::new(),
      ..at.clone()
    };
    match mercurial
      .iter_mut()
      .find(|((held_in, read_at), _)| *held_in == repository && *read_at == key)
    {
      Some((_, set)) if set.contains(&wanted) => {}
      Some((_, set)) => set.push(wanted),
      None => mercurial.push(((repository, key), vec![wanted])),
    }
  }
  let mut brought = 0u64;
  for ((repository, at), wanted) in &mercurial {
    super::hg_named::bring_back(*repository, (at.green, at.version), wanted)
      .await
      .map_err(super::read_through::ReadThroughRefused::Mercurial)?;
    brought = brought.saturating_add(u64::try_from(wanted.len()).unwrap_or(u64::MAX));
  }
  if brought > 0 {
    count_brought_back(brought, began);
  }
  Ok(pins)
}

/// Brings back the let-go window `window` of Subversion repository `repository`'s text `identity`, from byte `start`,
/// read through the node's store a piece at a time (`StoreHandle::text`, as `svn_texts::bring_whole` reads one), each
/// read waited for at most `read_through::PATIENCE_NS`; nothing to do for a window back already.
async fn bring_back_text_window(
  repository: slates_db::catalog::RepositoryId,
  identity: Identity,
  (window, start): (ContentId, u64),
) -> Result<(), super::read_through::ReadThroughRefused> {
  use super::read_through::ReadThroughRefused;
  let found = crate::state::with_state(|state| {
    let content = state.vcs_stores.content()?;
    let (_, _, width) = content.window_holding(&ContentId(identity.hash), identity.len, start)?;
    content
      .is_away(&window)
      .then(|| (state.vcs_disk.clone(), width))
  })
  .flatten();
  let Some((disk, width)) = found else {
    return Ok(());
  };
  let disk = disk.ok_or(ReadThroughRefused::NoDisk)?;
  let from = u128::from_be_bytes(repository.bytes);
  let end = start.saturating_add(width);
  let mut bytes = Vec::new();
  bytes
    .try_reserve_exact(usize::try_from(width).unwrap_or(usize::MAX))
    .map_err(|_| ReadThroughRefused::Refused(slates_vcs::ContentRefused::CountExhausted))?;
  let mut at = start;
  while at < end {
    let read = slates_rt::futures::within(super::read_through::PATIENCE_NS, async {
      loop {
        match disk.text(from, identity.hash, at).await {
          Err(slates_store::owner::OwnerError::Busy) => slates_rt::futures::yield_now().await,
          answered => break answered,
        }
      }
    })
    .await;
    let piece = match read {
      Ok(Some(Ok(Some(piece)))) if !piece.is_empty() => piece,
      Ok(Some(Ok(_))) => return Err(ReadThroughRefused::Missing { at }),
      Ok(Some(Err(refused))) => return Err(ReadThroughRefused::Owner(format!("{refused:?}"))),
      Ok(None) => {
        let _ = crate::state::with_state(|state| state.count(super::read_through::UNANSWERED, 1));
        return Err(ReadThroughRefused::Unanswered { at });
      }
      Err(refused) => return Err(ReadThroughRefused::Owner(format!("{refused:?}"))),
    };
    let want = usize::try_from(end.saturating_sub(at)).unwrap_or(usize::MAX);
    let taken = piece.get(..want.min(piece.len())).unwrap_or_default();
    bytes.extend_from_slice(taken);
    at = at.saturating_add(u64::try_from(taken.len()).unwrap_or(u64::MAX));
  }
  crate::state::with_state(|state| {
    let brought = super::within_budget(state, width, |stores| {
      let content = stores.content_mut().ok_or(ReadThroughRefused::NoStore)?;
      if !content.is_away(&window) {
        return Ok(false);
      }
      content
        .bring_back(&window, &bytes)
        .map(|()| true)
        .map_err(ReadThroughRefused::Refused)
    });
    // A read-back, its repository's to cache within its grant (`vcs_share::read_backs`, §4.17.5 "The cold trigger").
    if let Ok(true) = brought {
      crate::vcs_share::read_backs::brought_back(
        state,
        window,
        width,
        crate::vcs_share::read_backs::Holder::Text(repository),
      );
    }
    brought.map(drop)
  })
  .ok_or(ReadThroughRefused::NoStore)?
}

/// Green `green`'s current file `path` whole, its let-go windows brought back first where it is held by name
/// ([`bring_back_range`]): what an async reader of a whole file reads.
pub(crate) async fn read_file_brought(
  green: slates_db::catalog::VolumeId,
  path: &str,
) -> Option<Vec<u8>> {
  let named = crate::state::with_state(|state| {
    let engine = state.greens.get(&green)?;
    match engine.content(path)? {
      slates_merge::engine::Content::Named(identity) => Some(Some((identity, engine.head()))),
      slates_merge::engine::Content::Bytes(_) => Some(None),
    }
  })??;
  let _pins = match named {
    Some((identity, version)) => {
      let read = AwayRead {
        identity,
        offset: 0,
        len: identity.len,
        at: read_at(Some((green, path, version))),
      };
      Some(bring_back_reads(&[read]).await.ok()?)
    }
    None => None,
  };
  crate::state::with_state(|state| read_file(state, green, path))?
}

/// The text green `green` holds under `identity` whole, its let-go windows brought back first ([`bring_back_range`]).
pub(crate) async fn read_held_brought(
  green: slates_db::catalog::VolumeId,
  identity: slates_merge::held::Identity,
) -> Option<Vec<u8>> {
  let named =
    crate::state::with_state(|state| match state.greens.get(&green)?.held(&identity)? {
      slates_merge::engine::Content::Named(object) => Some(object),
      slates_merge::engine::Content::Bytes(_) => None,
    })?;
  let _pin = match named {
    Some(object) => Some(bring_back_range(object, (0, object.len)).await.ok()?),
    None => None,
  };
  crate::state::with_state(|state| read_held(state, green, &identity))?
}

/// A replay's named objects (A-866, A-869): the content plane, where the shard holds a content store; none without one.
pub(crate) struct ReplayObjects<'a> {
  /// The plane, where the shard holds a content store.
  pub(crate) plane: Option<PlaneObjects<'a>>,
}

impl Objects for ReplayObjects<'_> {
  fn read(&self, identity: &Identity, at: u64, into: &mut [u8]) -> Result<(), ObjectMissing> {
    match &self.plane {
      Some(plane) => plane.read(identity, at, into),
      None => Err(ObjectMissing {
        identity: *identity,
      }),
    }
  }
}

impl Objects for PlaneObjects<'_> {
  fn read(&self, identity: &Identity, at: u64, into: &mut [u8]) -> Result<(), ObjectMissing> {
    let missing = ObjectMissing {
      identity: *identity,
    };
    let wanted = u64::try_from(into.len()).map_err(|_| missing)?;
    if at.checked_add(wanted).is_none_or(|end| end > identity.len) {
      return Err(missing);
    }
    let root = self.store.root_of(&ContentId(identity.hash));
    let window = u64::try_from(self.store.window())
      .ok()
      .filter(|window| *window > 0)
      .ok_or(missing)?;
    let mut done = 0usize;
    while done < into.len() {
      let offset = at.saturating_add(u64::try_from(done).map_err(|_| missing)?);
      let index = offset.checked_div(window).ok_or(missing)?;
      let skip = offset.checked_rem(window).ok_or(missing)?;
      // The rest of this window, at most what is left to read: read straight into the run's buffer.
      let in_window = usize::try_from(window.saturating_sub(skip)).map_err(|_| missing)?;
      let room = into.get_mut(done..).ok_or(missing)?;
      let take = in_window.min(room.len());
      let piece = room.get_mut(..take).ok_or(missing)?;
      let skip = usize::try_from(skip).map_err(|_| missing)?;
      self
        .store
        .window_range_in((&root, identity.len, index), (skip, piece), self.kept_in)
        .ok_or(missing)?;
      done = done.saturating_add(take);
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used)]

  use slates_merge::engine::{Green, Increment, Judgment};
  use slates_merge::increment::{Base, VolumeOp, compose_volume_planned};
  use slates_merge::named::{Identity, NamedRun};

  use super::PlaneObjects;
  use crate::state::ShardState;

  /// Shape: the text's length: the 500 MB commit's shape scaled to a focused test, many copy slices long.
  const TEXT: usize = 8 << 20;

  /// The text, put into the shard's content plane as an intake streams it (`ContentStore::put_window`), and its identity.
  fn put_text(state: &mut ShardState, bytes: &[u8]) -> Identity {
    let ShardState {
      store, vcs_stores, ..
    } = state;
    // Lent the room the text brings, as a step's puts are (`vcs_store::lend`), then settled.
    crate::vcs_store::lend(store, vcs_stores, bytes.len() as u64 * 2);
    let id = {
      let lent = crate::vcs_store::Lent::new(&mut *store, vcs_stores);
      let content = lent.stores.content_mut().unwrap();
      let mut writing = content.begin_object();
      for piece in bytes.chunks(97 * 1024) {
        content.put_window(&mut writing, piece).unwrap();
      }
      content.finish_object(&mut writing).unwrap()
    };
    crate::vcs_store::settle_in(store, vcs_stores);
    Identity {
      hash: id.0,
      len: bytes.len() as u64,
    }
  }

  /// A work creating `path` from the whole of `object`, named.
  fn named_create(path: &str, object: Identity) -> Increment {
    let journal = vec![
      VolumeOp::Create {
        path: path.to_owned(),
      },
      VolumeOp::Extend {
        path: path.to_owned(),
        at: 0,
        len: object.len,
      },
    ];
    let (doc, _) = compose_volume_planned(&Base::of_files(Vec::new()), &journal).unwrap();
    let mut work = Increment {
      id: [5; 32],
      base: 0,
      doc,
      post_state: Vec::new(),
      evidence: Vec::new(),
      named: Default::default(),
    };
    work.push_named(NamedRun {
      identity: object,
      offset: 0,
      len: object.len,
    });
    work
  }

  /// A-760 part 2 (main's red: the 500 MB Subversion text's landing, scaled; Ada's sizing rule). Do: put an 8 MiB text
  /// into the shard's content plane; land a work creating a file of it, named, its run read a turn at a time (each turn
  /// lent the store, its copy slice at the shard's measured memcpy rate), then judged with `judge_named` and committed.
  /// Expect: more turns than the text has slices less one; no turn read more than its slice, nor ran past its budget by
  /// more than one turn's read; the green's file the text's name alone, no byte of it held beside the plane (A-760 (b)
  /// 4d; before, the very buffer the turns read into), its bytes the text's read through the plane; and the same bytes
  /// read with the shard's chunk store given in place of a lent one (a recovery's replay).
  #[test]
  fn t_a760_a_large_named_text_lands_a_slice_a_turn_held_once() {
    crate::test_store::audit_on_stored_shard("vcs_store-named_objects", |state| {
      let _repository = crate::git_push::tests::git_repository(state, "texts");
      let bytes: Vec<u8> = (0..TEXT)
        .map(|at| u8::try_from(at % 239).unwrap())
        .collect();
      let object = put_text(state, &bytes);
      let slice = state.config.copy_slice_bytes().max(1);
      let budget = state.config.step_quantum_ns() / 2;
      let mut work = named_create("big", object);
      let (mut turns, mut largest, mut longest) = (0u64, 0usize, 0u64);
      loop {
        turns += 1;
        let before = work.read_run(0).map_or(0, <[u8]>::len);
        let began = slates_rt::futures::now_ns();
        let done = crate::vcs_store::lent_state(state, |state| {
          let objects = PlaneObjects {
            store: state.vcs_stores.content().unwrap(),
            kept_in: &(),
          };
          work.read_step(&objects, slice).unwrap()
        });
        longest = longest.max(slates_rt::futures::now_ns().saturating_sub(began));
        largest = largest.max(work.read_run(0).unwrap().len() - before);
        if done {
          break;
        }
      }
      let mut green = Green::new();
      let Judgment::Accepted(judged) = green.judge_named(&mut work) else {
        panic!("not accepted");
      };
      green.commit_judged(judged).unwrap();
      assert_eq!(
        green.content("big"),
        Some(slates_merge::engine::Content::Named(object))
      );
      assert_eq!(green.named_bytes_held(), 0, "the file holds bytes");
      let file = super::whole_of(
        slates_merge::engine::ContentAt::Named(object),
        super::plane(&state.vcs_stores, &state.store.content).as_ref(),
        None,
      )
      .unwrap();
      let replayed = {
        let mut again = named_create("big", object);
        let objects = PlaneObjects {
          store: state.vcs_stores.content().unwrap(),
          kept_in: &state.store.content,
        };
        while !again.read_step(&objects, slice).unwrap() {}
        again.read_run(0) == Some(bytes.as_slice())
      };
      eprintln!(
        "A-760 {TEXT} bytes: {turns} turns of a {slice}-byte slice, the largest read {largest}, the longest turn \
         {longest} ns against a {budget} ns budget"
      );
      assert!(turns >= (TEXT as u64).div_ceil(slice), "{turns} turns");
      assert!(largest as u64 <= slice, "a turn read {largest} bytes");
      assert_eq!(file, bytes);
      assert!(replayed, "the replay's read differs");
    });
  }
}
