//! A Subversion repository's head texts held by the green, by content identity (S4 slice 2, step c1; A-727 (c); A-716;
//! rule (B): the green keeps the head's files, the repository store their history).
//!
//! **What it does.** Each text a file of the repository's head holds is held in the green's held table
//! (`slates_merge::held`) by path: the head file's own block, one more holder, so no byte is copied. A revision that
//! lands releases the texts the paths it changed held before it, a directory's files with it, and holds the texts they
//! hold after it, a copied directory's files with it ([`released_before`], [`held_after`], [`follow`]). A green made
//! again by a recovery holds none (the table is not in its checkpoint, A-716 slice 1c), so the recovery holds every
//! file of the head, in byte order, once the records are replayed ([`rehold_head`]): the same table every time.
//!
//! **Reading** (c3). [`HeadTexts`] is the Subversion sources' text source (`slates_svn::migration::TextSource`). A text
//! of the head is read from the green, which holds it (borrowed, no copy-out). Any other text is read from the one place
//! the records say it is kept, never tried in turn: a text a commit stored (main's ruling (B)) from the content store by
//! its identity, its length the records'; a text whose bytes a property list holds too from the records, the only text
//! bytes they hold (the KeptText move: the records keep no text). A text kept in neither is no text. A read takes a
//! piece at a time (`text_at`): the green's bytes from the offset, the store's window holding it, or the records' bytes
//! from it; nothing is made whole.
//!
//! **Room.** The table's entries are bounded by what the shard could admit, an entry its identity, its block and its
//! count: [`set_room`] derives it before each hold, so a hold past it is refused typed and counted, and the text read
//! from where the records keep it.

use std::borrow::Cow;

use slates_db::catalog::{RepositoryId, VolumeId as DbVolumeId};
use slates_merge::engine::Green;
use slates_svn::index::{Action, ChangedPath, ContentIdentity, Index, NodeKind};
use slates_svn::migration::{Repository as SvnRecords, TextReading, TextSource};

use crate::state::ShardState;

/// Counter: head texts the green holds by path (S4 c1).
pub(crate) const HELD: &str = "vcs_store.svn_texts.held";
/// Counter: holds or releases the green's table refused (its room, no file at the path): the text is read from the
/// records.
pub(crate) const REFUSED: &str = "vcs_store.svn_texts.refused";

/// Shape: what one entry of the green's held table holds, for its room: the identity, the block's index and the count,
/// and a map node's three words.
const ENTRY_BYTES: u64 =
  (size_of::<ContentIdentity>() + 2 * size_of::<u64>() + 3 * size_of::<usize>()) as u64;

/// Text pieces read through the node's changeset store for drives waiting on them (C2 stage B part 2; module docs,
/// "Reading"): each piece by its text and the offset it starts at, with how many waiting readers hold it. Bounded by
/// the readers waiting on the shard: a reader puts the piece it waited for ([`fetch_piece`]) and lets it go
/// ([`let_go`]) once it moves on from it: it waits for another, or its read ends (`svn_view::FetchedPiece`, A-876).
#[derive(Debug, Default)]
pub(crate) struct Pieces(std::collections::BTreeMap<(ContentIdentity, u64), (Vec<u8>, usize)>);

impl Pieces {
  /// How many pieces are held.
  #[cfg(test)]
  pub(crate) fn len(&self) -> usize {
    self.0.len()
  }

  /// The piece of `identity` holding `offset`, from `offset` on; none where no reader fetched it.
  pub(super) fn at(&self, identity: &ContentIdentity, offset: u64) -> Option<&[u8]> {
    let ((_, start), (bytes, _)) = self
      .0
      .range(..=(*identity, offset))
      .next_back()
      .filter(|((text, _), _)| text == identity)?;
    let skip = usize::try_from(offset.checked_sub(*start)?).ok()?;
    bytes.get(skip..).filter(|rest| !rest.is_empty())
  }
}

/// Counter: a text piece a waiting drive read through the node's changeset store (C2 stage B part 2). Format: a counter
/// name in the status report.
pub(crate) const PIECE_FETCHED: &str = "vcs_store.svn_texts.piece_fetched";
/// Counter: a piece the node's store could not give (no store, its owner gone, no such text): the waiting drive's
/// connection ends. Format: a counter name.
pub(crate) const PIECE_UNFETCHED: &str = "vcs_store.svn_texts.piece_unfetched";

/// Reads the text piece `wanted` of repository `repository` through the node's changeset store and puts it where the
/// repository's views read it ([`Pieces`]), held once more for the reader that waited; whether it did. A busy owner is
/// asked again after a yield. Counted.
pub(crate) async fn fetch_piece(
  repository: slates_db::catalog::RepositoryId,
  wanted: slates_svn::reads::Wanted,
) -> bool {
  let Some(Some(disk)) = crate::state::with_state(|state| state.vcs_disk.clone()) else {
    return false;
  };
  let from = u128::from_be_bytes(repository.bytes);
  let piece = loop {
    match disk.text(from, wanted.text, wanted.offset).await {
      Ok(Some(piece)) if !piece.is_empty() => break Some(piece),
      Err(slates_store::owner::OwnerError::Busy) => slates_rt::futures::yield_now().await,
      _ => break None,
    }
  };
  crate::state::with_state(|state| {
    let Some(piece) = piece else {
      state.count(PIECE_UNFETCHED, 1);
      return false;
    };
    let held = state
      .vcs_stores
      .svn_pieces
      .0
      .entry((wanted.text, wanted.offset))
      .or_insert((piece, 0));
    held.1 = held.1.saturating_add(1);
    state.count(PIECE_FETCHED, 1);
    true
  })
  .unwrap_or(false)
}

/// Reads text `identity` of `len` bytes of repository `repository` whole through the node's changeset store into the
/// content store, a piece at a time, made whole there with one hold (C2 stage B part 2 (b)): for a reader that needs a
/// stored text whole in one step (a commit's green increment). Whether it did, its bytes its identity's; counted as a
/// piece fetched each. A busy owner is asked again after a yield.
pub(crate) async fn bring_whole(
  repository: slates_db::catalog::RepositoryId,
  identity: ContentIdentity,
  len: u64,
) -> bool {
  let Some(Some(disk)) = crate::state::with_state(|state| state.vcs_disk.clone()) else {
    return false;
  };
  let from = u128::from_be_bytes(repository.bytes);
  // Begun with its length at its first piece, the text's window buffer is reserved once (goal 93, svn finding (2)).
  let mut writing: Option<slates_vcs::Writing> = None;
  let mut offset = 0u64;
  while offset < len {
    let piece = match disk.text(from, identity, offset).await {
      Ok(Some(piece)) if !piece.is_empty() => piece,
      Err(slates_store::owner::OwnerError::Busy) => {
        slates_rt::futures::yield_now().await;
        continue;
      }
      _ => break,
    };
    let put = crate::state::with_state(|state| put_brought(state, (&mut writing, len), &piece));
    if put != Some(true) {
      break;
    }
    offset = offset.saturating_add(u64::try_from(piece.len()).unwrap_or(u64::MAX));
  }
  crate::state::with_state(|state| finish_brought(state, writing, (offset, len), identity))
    .unwrap_or(false)
}

/// [`bring_whole`] in a recovery, where the shard's state is its caller's (`verbs::rebuild_recovered`) rather than
/// installed for its tasks: the same pieces, the same put.
pub(crate) async fn bring_whole_in(
  state: &mut ShardState,
  repository: slates_db::catalog::RepositoryId,
  identity: ContentIdentity,
  len: u64,
) -> bool {
  let Some(disk) = state.vcs_disk.clone() else {
    return false;
  };
  let from = u128::from_be_bytes(repository.bytes);
  let mut writing: Option<slates_vcs::Writing> = None;
  let mut offset = 0u64;
  while offset < len {
    let piece = match disk.text(from, identity, offset).await {
      Ok(Some(piece)) if !piece.is_empty() => piece,
      Err(slates_store::owner::OwnerError::Busy) => {
        slates_rt::futures::yield_now().await;
        continue;
      }
      _ => break,
    };
    if !put_brought(state, (&mut writing, len), &piece) {
      break;
    }
    offset = offset.saturating_add(u64::try_from(piece.len()).unwrap_or(u64::MAX));
  }
  finish_brought(state, writing, (offset, len), identity)
}

/// One piece of a text being brought whole into the content store ([`bring_whole`]): counted, kept a window at a time
/// within the room it is lent, the text's object begun with its length at its first piece. Whether it was kept.
fn put_brought(
  state: &mut ShardState,
  (writing, len): (&mut Option<slates_vcs::Writing>, u64),
  piece: &[u8],
) -> bool {
  let brought = u64::try_from(piece.len()).unwrap_or(u64::MAX);
  state.count(PIECE_FETCHED, 1);
  state.count(BYTES_BROUGHT, brought);
  crate::vcs_store::within_budget(state, brought, |stores| {
    stores.content_mut().is_some_and(|content| {
      let writing = writing.get_or_insert_with(|| content.begin_object_of_length(len));
      content.put_window(writing, piece).is_ok()
    })
  })
}

/// The end of a text brought whole ([`bring_whole`]): its object finished with one hold where every byte came and its
/// bytes are its identity's, else given up, counted. Whether it is held.
fn finish_brought(
  state: &mut ShardState,
  writing: Option<slates_vcs::Writing>,
  (offset, len): (u64, u64),
  identity: ContentIdentity,
) -> bool {
  let made = crate::vcs_store::within_budget(state, 0, |stores| {
    let Some(content) = stores.content_mut() else {
      return false;
    };
    let mut writing = writing.unwrap_or_else(|| content.begin_object_of_length(len));
    if offset != len {
      content.abandon_object(writing);
      return false;
    }
    match content.finish_object(&mut writing) {
      Ok(made) if made == slates_vcs::ContentId(identity) => true,
      Ok(made) => {
        let _ = content.release(&made);
        false
      }
      // A refused finish leaves what was kept held by the writer, for its caller to abandon (`finish_object`'s
      // errors): dropped, its windows stayed charged and readable (codex-review, 2026-11-22: 262,144 bytes of one
      // refused text).
      Err(_) => {
        content.abandon_object(writing);
        false
      }
    }
  });
  if !made {
    state.count(PIECE_UNFETCHED, 1);
  }
  made
}

/// Counter: a text a Subversion green's recovery brought back from the node's changeset store before its replay
/// ([`bring_back_named`]): a text its checkpoint (A-869) or a chain entry names that the restored content store does
/// not hold whole. That is the head's texts once a checkpoint names them, and any entry's texts past the checkpoint,
/// which a stop before a deferred checkpoint commits (`merge_service`'s `checkpoint_in_turns`) leaves in the chain.
/// Format: a counter name in the status report.
pub(crate) const NAMED_BROUGHT_BACK: &str = "vcs_store.svn_texts.named_brought_back";

/// A-760's recovery pre-pass (main 01a20182's rulings of 2026-11-22 ~11:35 and ~11:50, (i)): the texts that green
/// `green`'s checkpoint (A-869) and the entries of its chain name (A-760) and the content store does not hold whole,
/// brought back whole from the node's changeset store before the green's replay, a text a recovery unit, so the restore
/// and the replay find every name and their `ContentMissing` stays a true fence. What it brought, each holding one count
/// of the content store, for the caller to give back once the green is replayed. Nothing but for a Subversion
/// repository's member.
///
/// **Bound.** The chain holds the entries past the green's checkpoint alone, and an import's batch checkpoints its green
/// as it lands, so what the entries bring is at most one batch's texts, bounded by the batch's admission
/// (`svn_commit::staging`). The checkpoint names the head's texts it no longer carries (A-869): what was its bytes, in
/// the checkpoint's own object before, is brought into the content store instead, each a text a recovery unit within
/// the room each is lent, and given back once the green is replayed.
///
/// **Why it can be found.** An entry naming a text commits only once the changeset holding that text is durable on the
/// node's store: `staging::Staging::land` awaits `landing::land_changeset` before `ImportBatch::commit` appends the
/// entry, and a client's commit lands its changeset before its entry likewise (`svn_commit::landed_with`). The texts are
/// the changeset's `Payload` there.
pub(crate) async fn bring_back_named(
  state: &mut ShardState,
  green: slates_db::catalog::VolumeId,
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> Vec<ContentIdentity> {
  let Some((repository, wanted)) = to_bring(state, green) else {
    return Vec::new();
  };
  let mut brought = Vec::with_capacity(wanted.len());
  for (identity, len) in wanted {
    if bring_whole_in(state, repository, identity, len).await {
      state.count(NAMED_BROUGHT_BACK, 1);
      brought.push(identity);
    }
    slices.unit_done().await;
  }
  brought
}

/// What a replay of green `green` needs brought back first ([`bring_back_named`], [`bring_back_named_live`]): its
/// Subversion repository and the named texts its checkpoint and chain name that the content store does not hold whole
/// ([`named_lacking`]), the content store made where it is not yet. Made as a first repository makes it: at a recovery,
/// the image's windows and holds it takes in were set before the replays (`verbs::restore_held_image`, which runs
/// before the records are rebuilt). None with nothing to bring, for a green of no Subversion repository, or with no
/// store to bring into.
fn to_bring(
  state: &mut ShardState,
  green: slates_db::catalog::VolumeId,
) -> Option<(
  slates_db::catalog::RepositoryId,
  Vec<(ContentIdentity, u64)>,
)> {
  let repository = subversion_of(state, green)?;
  let wanted = named_lacking(state, green);
  if wanted.is_empty() {
    return None;
  }
  let window = state.store.content.chunk_bytes();
  state.vcs_stores.content_made(window).ok()?;
  Some((repository, wanted))
}

/// [`bring_back_named`] on a live shard: a takeover's re-adoption, which replays a green's own checkpoint and chain
/// onto the engine it is handed (`green_adopt::readopt_with`). The same plan ([`to_bring`]), each text brought whole
/// through the node's changeset store with the shard's state taken a piece at a time ([`bring_whole`]), so it runs as a
/// task of the owner shard between its other work. What it brought, each holding one count of the content store, for
/// the caller to give back once the green is installed or fenced.
pub(crate) async fn bring_back_named_live(
  green: slates_db::catalog::VolumeId,
) -> Vec<ContentIdentity> {
  let Some(Some((repository, wanted))) = crate::state::with_state(|state| to_bring(state, green))
  else {
    return Vec::new();
  };
  let mut brought = Vec::with_capacity(wanted.len());
  for (identity, len) in wanted {
    if bring_whole(repository, identity, len).await {
      let _ = crate::state::with_state(|state| state.count(NAMED_BROUGHT_BACK, 1));
      brought.push(identity);
    }
  }
  brought
}

/// The Subversion repository green `green` is a member of, if it is one's.
fn subversion_of(
  state: &ShardState,
  green: slates_db::catalog::VolumeId,
) -> Option<slates_db::catalog::RepositoryId> {
  let repositories = state.db.partition().repositories();
  let member = repositories.member(green)?;
  let record = repositories.repository(member.repository)?;
  (record.tool == slates_db::catalog::VcsTool::Subversion).then_some(member.repository)
}

/// The texts green `green`'s checkpoint (A-869) and chain entries name that the content store does not hold whole
/// (lacks, or holds with a window let go), each once, with its length.
pub(crate) fn named_lacking(
  state: &ShardState,
  green: slates_db::catalog::VolumeId,
) -> Vec<(ContentIdentity, u64)> {
  named_texts(state, green)
    .into_iter()
    .filter(|(_, _, whole)| !whole)
    .map(|(identity, len, _)| (identity, len))
    .collect()
}

/// Lets go, after its replay, each landed text green `green`'s checkpoint and chain name that its recovery found held
/// whole (`found_whole`, read before the replay; its windows restored resident with the shard's image): what the
/// recovery did not bring back, so its give-back never lets it go (TSan diag 45557094379: the successor held the text
/// whole at its start, fetched nothing, let nothing go, and kept it in RAM for good). Each is landed, its changeset
/// durable on the node's store before any entry named it, so its first reader brings it back (main's ruling (B)).
pub(crate) fn let_go_found_whole(state: &mut ShardState, found_whole: &[ContentIdentity]) {
  if !found_whole.is_empty() {
    state.count(
      NAMED_FOUND_WHOLE,
      u64::try_from(found_whole.len()).unwrap_or(u64::MAX),
    );
    crate::vcs_store::name_holds::let_go_landed_texts(state, found_whole);
  }
}

/// The landed texts green `green`'s checkpoint and chain name that the content store holds whole now, for its recovery
/// to let go once it has replayed ([`let_go_found_whole`]); none for a green of no Subversion repository.
pub(crate) fn named_found_whole(
  state: &ShardState,
  green: slates_db::catalog::VolumeId,
) -> Vec<ContentIdentity> {
  if subversion_of(state, green).is_none() {
    return Vec::new();
  }
  named_texts(state, green)
    .into_iter()
    .filter(|(_, _, whole)| *whole)
    .map(|(identity, _, _)| identity)
    .collect()
}

/// Counter: a named landed text a recovery found held whole and let go after its replay ([`let_go_found_whole`]).
/// Format: a counter name in the daemon's status report.
pub(crate) const NAMED_FOUND_WHOLE: &str = "vcs_store.svn_texts.named_found_whole";

/// The texts green `green`'s checkpoint (A-869) and chain entries name, each once, with its length and whether the
/// content store holds it whole.
fn named_texts(
  state: &ShardState,
  green: slates_db::catalog::VolumeId,
) -> Vec<(ContentIdentity, u64, bool)> {
  let mut wanted: Vec<(ContentIdentity, u64, bool)> = Vec::new();
  let mut seen: std::collections::BTreeSet<ContentIdentity> = std::collections::BTreeSet::new();
  // Held whole, or brought: a restored store keeps the name of a text whose window was let go, and the restore and the
  // replay read every byte (codex-review's restart of 2026-11-22: r1 fenced `ContentMissing` for one six-byte window).
  let mut want = |identity: slates_merge::named::Identity| {
    if !seen.insert(identity.hash) {
      return;
    }
    let held = state
      .vcs_stores
      .content()
      .is_some_and(|content| content.holds_whole(&slates_vcs::ContentId(identity.hash)));
    wanted.push((identity.hash, identity.len, held));
  };
  let partition = state.db.partition();
  if let Some((_, named)) = partition.green_checkpoint(green)
    && let Some(bytes) = crate::merge_service::checkpoint_bytes(state, green, &named)
    && let Ok(checkpoint) = slates_merge::checkpoint::Checkpoint::decode(&bytes)
  {
    for file in &checkpoint.named {
      want(file.identity);
    }
  }
  for bytes in partition.green_chain(green).iter() {
    if slates_merge::push::DeferredPush::is_record(bytes) {
      continue;
    }
    let Ok((inputs, carried)) = slates_merge::chain_entry::placement(bytes) else {
      continue;
    };
    let increment = match carried {
      Some(carried) => Cow::Borrowed(carried),
      None => match crate::merge_service::increment_of(state, green, &inputs) {
        Some(increment) => increment,
        None => continue,
      },
    };
    let Ok(entry) = slates_merge::chain_entry::decode(bytes, &increment) else {
      continue;
    };
    // A run the replay's apply names whole is never read for it (A-887, `Increment::replay_reads`): nothing to bring.
    let reads = entry.increment.replay_reads(&entry.applied);
    for (at, run) in entry.increment.named.runs.iter().enumerate() {
      if reads.get(at).copied().unwrap_or(true) {
        want(run.identity);
      }
    }
  }
  wanted
}

/// Counter: the bytes a reader that needs a text whole brought through the node's changeset store ([`bring_whole`]: a
/// commit's increment, a copy's restore, a hook view, an import's delta base). Format: a counter name in the status
/// report.
pub(crate) const BYTES_BROUGHT: &str = "vcs_store.svn_texts.bytes_brought";

/// Lets go the reader's hold on the piece `wanted` it fetched ([`fetch_piece`]), the piece gone with its last hold.
pub(crate) fn let_go(state: &mut ShardState, wanted: slates_svn::reads::Wanted) {
  let pieces = &mut state.vcs_stores.svn_pieces.0;
  let key = (wanted.text, wanted.offset);
  if let Some(held) = pieces.get_mut(&key) {
    held.1 = held.1.saturating_sub(1);
    if held.1 == 0 {
      pieces.remove(&key);
    }
  }
}

/// The Subversion sources' texts (module docs, "Reading"): the green's held table first, then the records.
pub(crate) struct HeadTexts<'a> {
  /// The repository's green, when it is held here.
  pub(crate) green: Option<&'a Green>,
  /// The shard's content store, its chunks lent (`crate::vcs_store::lent_state`): where a text a commit stored lives
  /// (§4.17.1 "A push's intake"; the records keep it as a reference).
  pub(crate) store: Option<&'a slates_vcs::ContentStore>,
  /// The repository's records.
  pub(crate) records: &'a SvnRecords,
  /// The pieces waiting drives fetched through the node's store, where a reader that can wait reads a stored text the
  /// content store does not hold ([`fetch_piece`]); none for a reader that cannot wait.
  pub(crate) pieces: Option<&'a Pieces>,
}

/// The bytes of the text `green` holds under `identity` (A-716's table), where it holds its bytes: a text it holds by
/// its content-plane object alone (A-760 (b) 4b) is read where the records keep it stored, the content store, by the
/// same identity, as any text the green does not hold is.
fn held_bytes<'g>(
  green: &'g slates_merge::engine::Green,
  identity: &ContentIdentity,
) -> Option<&'g [u8]> {
  green
    .held(identity)
    .and_then(|content| content.bytes().ok())
}

impl HeadTexts<'_> {
  /// The length of text `identity` where it is read from the content store: a text the records keep stored, or one the
  /// green holds by its object's name alone (A-760 (b) 4c: a text a batch staged and has not landed yet, which the
  /// records do not name stored, is read from the content store all the same).
  fn stored_len(&self, identity: &ContentIdentity) -> Option<u64> {
    self
      .records
      .stored_len(identity)
      .or_else(|| match self.green?.held(identity)? {
        slates_merge::engine::Content::Named(object) => Some(object.len),
        slates_merge::engine::Content::Bytes(_) => None,
      })
  }
}

impl TextSource for HeadTexts<'_> {
  /// The head's text from the green; any other from where the records keep it (module docs, "Reading").
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    if let Some(bytes) = self.green.and_then(|green| held_bytes(green, identity)) {
      return Some(Cow::Borrowed(bytes));
    }
    match self.stored_len(identity) {
      Some(len) => {
        let store = self.store?;
        let id = slates_vcs::ContentId(*identity);
        store.object_bytes(&store.root_of(&id), len).map(Cow::Owned)
      }
      None => self.records.text(identity),
    }
  }

  /// The text from `offset`, a piece at a time (a fetch's read, main's order after w-bugs' runs 10/11): from the green,
  /// borrowed to the text's end; else the content store's window holding `offset`; else the records' own bytes from it.
  /// Nothing is made whole.
  fn text_at(&self, identity: &ContentIdentity, offset: u64) -> Option<Cow<'_, [u8]>> {
    if let Some(bytes) = self.green.and_then(|green| held_bytes(green, identity)) {
      return bytes
        .get(usize::try_from(offset).ok()?..)
        .map(Cow::Borrowed);
    }
    match self.stored_len(identity) {
      Some(len) => {
        if offset >= len {
          return (offset == len).then_some(Cow::Borrowed(&[]));
        }
        let held = self
          .store
          .and_then(|store| store.run_at(&slates_vcs::ContentId(*identity), len, offset));
        // A stored text the content store does not hold is read through the node's store (C2 stage B part 2).
        held.or_else(|| self.pieces?.at(identity, offset).map(Cow::Borrowed))
      }
      None => self.records.text_at(identity, offset).map(Cow::Owned),
    }
  }

  /// A stored text's piece neither the green, the content store nor the fetched pieces hold, for a reader that can
  /// wait: it is read through the node's changeset store (C2 stage B part 2; [`fetch_piece`]).
  fn fetchable(&self, identity: &ContentIdentity, offset: u64) -> bool {
    self.pieces.is_some()
      && self
        .records
        .stored_len(identity)
        .is_some_and(|len| offset < len)
      && self.text_at(identity, offset).is_none()
  }

  /// [`TextSource::text_at`] for a reader keeping `cache` across its steps: the green's and the content store's runs
  /// lent in place as `text_at` lends them; a text the records hold, lent in place from them.
  fn text_in<'a>(
    &'a self,
    identity: &ContentIdentity,
    offset: u64,
    cache: &'a mut slates_svn::reads::WindowCache,
  ) -> Option<Cow<'a, [u8]>> {
    let kept_by_records = self.stored_len(identity).is_none()
      && self
        .green
        .and_then(|green| held_bytes(green, identity))
        .is_none();
    if kept_by_records {
      return self.records.text_in(identity, offset, cache);
    }
    self.text_at(identity, offset)
  }

  /// A step of the same order: the green's held text a window's room copied, the content store's a window of the
  /// store read (`ContentStore::window_at`, from the window the bytes read so far reach), the records' own bytes a
  /// window's room copied.
  fn read_step(&self, identity: &ContentIdentity, reading: &mut TextReading) -> Option<bool> {
    if let Some(bytes) = self.green.and_then(|green| held_bytes(green, identity)) {
      return Some(reading.copy_step(bytes));
    }
    let Some(len) = self.stored_len(identity) else {
      return TextSource::read_step(self.records, identity, reading);
    };
    let read = reading.read();
    if read >= len {
      return Some(true);
    }
    let wanted = usize::try_from(len.saturating_sub(read)).ok()?;
    // A stored text is read from the content store; one it does not hold, through the node's store (C2 stage B part 2):
    // the piece a waiting reader fetched.
    if let Some(store) = self.store {
      let root = store.root_of(&slates_vcs::ContentId(*identity));
      let window = u64::try_from(store.window()).ok()?;
      if let Some(piece) = store.window_at(&root, len, read.checked_div(window)?) {
        let skip = usize::try_from(read.checked_rem(window)?).ok()?;
        let rest = piece.get(skip..)?;
        return Some(reading.append(rest.get(..wanted).unwrap_or(rest), len));
      }
    }
    let rest = self.pieces?.at(identity, read)?;
    Some(reading.append(rest.get(..wanted).unwrap_or(rest), len))
  }
}

/// What a Subversion repository's sessions read (§4.17.4; main's ruling (B)): its records, with their texts read through
/// [`HeadTexts`].
pub(crate) type SvnView<'a> = slates_svn::migration::WithTexts<'a, HeadTexts<'a>>;

/// A file's text identity and its path in the green, as a walk finds it.
type FileText = (ContentIdentity, String);

/// The files at and beneath `path` in `revision`, each with its text, appended to `out` in byte order; a file with no
/// text (empty) is left out. A path the index cannot read adds nothing.
fn files_at(index: &Index, path: &str, revision: u64, out: &mut Vec<FileText>) {
  let mut stack = vec![path.to_owned()];
  while let Some(at) = stack.pop() {
    let Ok(Some(node)) = index.node(&at, revision) else {
      continue;
    };
    match node.kind() {
      NodeKind::File => {
        if let Some(text) = node.text() {
          out.push((text.identity, green_path(&at).to_owned()));
        }
      }
      NodeKind::Directory => {
        let Ok(Some(mut names)) = index.children(&at, revision) else {
          continue;
        };
        names.sort_unstable_by(|left, right| right.cmp(left));
        for name in names {
          stack.push(if at == "/" {
            format!("/{name}")
          } else {
            format!("{at}/{name}")
          });
        }
      }
    }
  }
}

/// The file at `path` in `revision` with its text, appended to `out`; a directory (whose property change changes no
/// file beneath it), an empty file or a path the index cannot read adds nothing.
fn file_at(index: &Index, path: &str, revision: u64, out: &mut Vec<FileText>) {
  if let Ok(Some(node)) = index.node(path, revision)
    && node.kind() == NodeKind::File
    && let Some(text) = node.text()
  {
    out.push((text.identity, green_path(path).to_owned()));
  }
}

/// A Subversion path's place in the green: without its leading `/`.
fn green_path(path: &str) -> &str {
  path.strip_prefix('/').unwrap_or(path)
}

/// The texts the paths `changes` changes held at `youngest`, before the revision applies: a modified file's, and every
/// file's at or beneath a deleted or replaced path.
pub(crate) fn released_before(
  records: &SvnRecords,
  changes: &[ChangedPath],
  youngest: u64,
) -> Vec<FileText> {
  let index = records.index();
  let mut released = Vec::new();
  for change in changes {
    match change.action {
      Action::Delete | Action::Replace => files_at(index, &change.path, youngest, &mut released),
      Action::Modify => file_at(index, &change.path, youngest, &mut released),
      Action::Add => {}
    }
  }
  once_a_path(released)
}

/// The texts the paths `changes` changes hold at `revision`, once it applied: a modified file's, and every file's at or
/// beneath an added or replaced path (a copied directory's files with it).
pub(crate) fn held_after(
  records: &SvnRecords,
  changes: &[ChangedPath],
  revision: u64,
) -> Vec<FileText> {
  let index = records.index();
  let mut held = Vec::new();
  for change in changes {
    match change.action {
      Action::Add | Action::Replace => files_at(index, &change.path, revision, &mut held),
      Action::Modify => file_at(index, &change.path, revision, &mut held),
      Action::Delete => {}
    }
  }
  once_a_path(held)
}

/// `files` with each path once, in byte order: a path two rows reach (a file changed beneath a directory the same
/// revision copied) holds its text once.
fn once_a_path(files: Vec<FileText>) -> Vec<FileText> {
  let by_path: std::collections::BTreeMap<String, ContentIdentity> = files
    .into_iter()
    .map(|(identity, path)| (path, identity))
    .collect();
  by_path
    .into_iter()
    .map(|(path, identity)| (identity, path))
    .collect()
}

/// Sets the green's held room from what the shard could admit now (module docs, "Room").
fn set_room(state: &mut ShardState, green: DbVolumeId) {
  let room = usize::try_from(state.store.admittable() / ENTRY_BYTES).unwrap_or(usize::MAX);
  if let Some(mut engine) = state.greens.get_mut(&green) {
    let held = engine.held_count();
    engine.set_held_room(held.saturating_add(room));
  }
}

/// A landed revision followed in the green's held table: each of `released` let go, then each of `held` held by its
/// path, the head's file's own block. A refusal is counted, and that text read from the records.
pub(crate) fn follow(
  state: &mut ShardState,
  green: DbVolumeId,
  released: &[FileText],
  held: &[FileText],
) {
  let mut at = 0;
  follow_some(state, green, (released, held), (&mut at, u64::MAX));
}

/// [`follow`] a slice at a time (goal 93: an import's landing followed 9,000 paths in one step): from `at`, a place
/// among `released` then `held`, until the shard's clock passes `deadline_ns`, at least one; `at` moved past those
/// followed. Whether every one is followed. The held room is set at the first.
pub(crate) fn follow_some(
  state: &mut ShardState,
  green: DbVolumeId,
  (released, held): (&[FileText], &[FileText]),
  (at, deadline_ns): (&mut usize, u64),
) -> bool {
  if *at == 0 {
    set_room(state, green);
  }
  let total = released.len().saturating_add(held.len());
  let (mut refused, mut holds) = (0u64, 0u64);
  if let Some(mut engine) = state.greens.get_mut(&green) {
    while *at < total {
      let place = *at;
      *at = place.saturating_add(1);
      match place.checked_sub(released.len()) {
        None => {
          if let Some((identity, _)) = released.get(place)
            && engine.release_held(identity).is_err()
          {
            refused = refused.saturating_add(1);
          }
        }
        Some(place) => {
          if let Some((identity, path)) = held.get(place) {
            match engine.hold_path(*identity, path) {
              Ok(()) => holds = holds.saturating_add(1),
              Err(_) => refused = refused.saturating_add(1),
            }
          }
        }
      }
      if slates_rt::futures::now_ns() >= deadline_ns {
        break;
      }
    }
  } else {
    *at = total;
  }
  state.count(HELD, holds);
  state.count(REFUSED, refused);
  *at >= total
}

/// Holds every file of repository `id`'s head in its green, in byte order, where the green holds none (module docs: a
/// green a recovery made again). A green that holds texts already keeps its table.
pub(crate) fn rehold_head(state: &mut ShardState, id: RepositoryId, green: DbVolumeId) {
  let fresh = state
    .greens
    .get(&green)
    .is_some_and(|engine| engine.held_count() == 0);
  if !fresh {
    return;
  }
  let Some(records) = state.vcs_stores.subversion_records(id) else {
    return;
  };
  let mut head = Vec::new();
  files_at(records.index(), "/", records.youngest(), &mut head);
  follow(state, green, &[], &head);
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_svn::migration::Room;

  /// A-865. Withhold admission between reading a landed text and finishing its object. Finishing
  /// refuses, and the first completed window must no longer consume room or answer a read.
  #[test]
  fn a_refused_subversion_text_bring_returns_its_completed_windows() {
    let daemon = crate::daemon::audit_daemon_configured(|_| {});
    let (finished, before, after, still_readable, window) =
      crate::daemon::observe_first(&daemon, |state| {
        let window = state.store.content.chunk_bytes();
        state.vcs_stores.content_made(window).unwrap();
        let before = state.vcs_stores.content().unwrap().used();
        let mut text = vec![b'x'; window];
        let leaf = slates_vcs::ContentId::of(&text);
        text.push(b'y');
        let len = text.len() as u64;
        let mut writing = None;
        assert!(put_brought(state, (&mut writing, len), &text));
        let hold = state.store.budget.hold();
        let room = state.store.admittable();
        state.store.budget.set_hold(hold.saturating_add(room));
        let finished = finish_brought(state, writing, (len, len), *blake3::hash(&text).as_bytes());
        state.store.budget.set_hold(hold);
        let (after, still_readable) = crate::vcs_store::within_budget(state, 0, |stores| {
          let content = stores.content().unwrap();
          (content.used(), content.bytes_of(&leaf).is_some())
        });
        (finished, before, after, still_readable, window)
      });
    daemon.stop();
    assert!(!finished, "the final byte exceeded admission");
    assert_eq!(
      (after, still_readable),
      (before, false),
      "one refused text with a {window}-byte window"
    );
  }

  /// A-761 with main's item 2 of 2026-11-20 (a text read a bounded step at a time). Do: put a 10,000-byte text into a
  /// content store of 4,096-byte windows; land revision 1 with `/trunk/a.txt` naming that text as stored (the records
  /// keep its length, no bytes); read it through `HeadTexts` a step at a time, and whole. Expect: the text, in three
  /// steps, a store window each; the whole read the same bytes.
  #[test]
  fn a_stored_text_is_read_a_store_window_a_step() {
    let window = 4_096;
    let bytes: Vec<u8> = (0..10_000u32).map(|at| (at % 253) as u8).collect();
    let mut store = slates_vcs::ContentStore::new(1 << 20, window).unwrap();
    let put = store.put_object(&bytes).unwrap();
    let identity = *blake3::hash(&bytes).as_bytes();
    assert_eq!(put.0, identity);
    let uuid = slates_svn::dump::Uuid::parse(b"00000000-0000-4000-8000-00000000a761").unwrap();
    let room = Room {
      entries: 1 << 10,
      bytes: 1 << 20,
    };
    let mut records = SvnRecords::new(uuid, b"2026-11-20T00:00:00.000000Z", room).unwrap();
    let mut revision = crate::repository::svn_commit::tests::first();
    let file = revision
      .record
      .changes
      .iter_mut()
      .find(|row| row.path == "/trunk/a.txt")
      .unwrap();
    let alpha = file.text.unwrap().identity;
    file.text = Some(slates_svn::commit::drive::text_of(&bytes).unwrap());
    revision.contents.remove(&alpha);
    revision
      .stored
      .insert(identity, u64::try_from(bytes.len()).unwrap());
    // The revision's other texts stored too, as a landing stores them (the records keep none, A-842); not read here.
    let _ = records.take_texts(&mut revision);
    let checked = records.check(revision).unwrap();
    records.apply(checked).unwrap();
    let texts = HeadTexts {
      green: None,
      store: Some(&store),
      records: &records,
      pieces: None,
    };
    let mut reading = TextReading::new();
    let mut steps = 0;
    while !texts.read_step(&identity, &mut reading).unwrap() {
      steps += 1;
      assert!(steps < 10, "the reading never ended");
    }
    assert_eq!((steps + 1, reading.bytes()), (3, bytes.as_slice()));
    assert_eq!(texts.text(&identity).as_deref(), Some(bytes.as_slice()));
  }
}
