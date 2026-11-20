//! The holder's side of the content plane (§4.10 "Content replication", §4.8 mechanism 1): one content
//! request answered on the shard that keeps this node's hold. Authority for the object comes first
//! (AUD-29-45), then the retention rule at the door (`crate::content_retention::admits`), the verified hold,
//! and the retention rule over what a held put superseded (AUD-29-43). The fleet's record session and the
//! daemon's test hook both serve through [`serve`], so a test drives the production path with only the
//! authority decision replaced. An acknowledgement is answered only once the shard's recovery image carrying
//! the put is committed into anchor-owned RAM (AUD-29-59), so a warm restart keeps every acknowledged
//! replica.

use std::collections::BTreeMap;

use slates_cluster::content::ContentAccess;
use slates_db::register::{
  Acceptor, HostId, ObjectId, PersistedAcceptor, RegionId, RegionalConfiguration, RootConfiguration,
};
use slates_wire::Wire;

use crate::state::ShardState;

/// The shard memory a content hold borrows for one operation (AUD-29-43): the shard store's chunk arena, its
/// byte budget and its metadata ledger.
pub(crate) fn hold_space(
  store: &mut slates_vfs::volume::Store,
) -> slates_cluster::content::HoldSpace<'_> {
  slates_cluster::content::HoldSpace {
    arena: store.content.arena_mut(),
    budget: &mut store.budget,
    metadata: &mut store.metadata,
  }
}

/// What a holder knows of the regions, for a put from the owner of an object homed in another region
/// (`docs/wip/mirroring.md` decision 2): the root configuration, each region's declared mirror, every member's
/// region, and the holder's own.
pub(crate) struct MirrorView<'a> {
  /// The root group's committed configuration: homes moved and regions promoted.
  pub(crate) root: &'a RootConfiguration,
  /// Each region's declared mirror (the manifest's `mirrors`).
  pub(crate) mirrors: &'a BTreeMap<RegionId, RegionId>,
  /// Every member's declared region.
  pub(crate) regions: &'a BTreeMap<HostId, RegionId>,
  /// This holder's region.
  pub(crate) own: RegionId,
}

impl MirrorView<'_> {
  /// Whether `peer` may place `object`'s content here as a mirror copy (`slates_db::mirror::admits_mirror_put`):
  /// only a placement, only at the home's declared mirror, only from a member of the object's standing home.
  pub(crate) fn admits_put(&self, peer: HostId, access: ContentAccess, object: ObjectId) -> bool {
    access == ContentAccess::Place
      && slates_db::mirror::admits_mirror_put(
        self.root,
        self.mirrors,
        self.regions,
        self.own,
        peer,
        object,
      )
      .is_ok()
  }
}

/// Serves one content request as `local`, with `authorized` deciding the access asked for the object named
/// from the committed configuration and this holder's records. Returns the reply: an empty one for
/// anything refused, malformed, unverifiable or unheld.
pub(crate) fn serve(
  state: &mut ShardState,
  local: HostId,
  request: &[u8],
  authorized: impl FnOnce(
    &RegionalConfiguration,
    &BTreeMap<ObjectId, Acceptor>,
    &MirrorView<'_>,
    ContentAccess,
    ObjectId,
  ) -> bool,
) -> Vec<u8> {
  let (council, records) = (&state.council, state.holder_records.as_map());
  let mirror = MirrorView {
    root: state.root.configuration(),
    mirrors: &state.region_mirrors,
    regions: &state.node_regions,
    own: state
      .node_regions
      .get(&local)
      .copied()
      .unwrap_or(RegionId(0)),
  };
  let unauthorized_before = state.held_content.unauthorized();
  let (reply, held) = state.held_content.serve(
    &mut hold_space(&mut state.store),
    local,
    request,
    |access, object| authorized(council.configuration(), records, &mirror, access, object),
    |object, sequence, manifest| {
      crate::content_retention::admits(records, local, object, sequence, manifest)
    },
  );
  // A request the access rule refused is counted (the hold answers it with the same empty reply as anything unheld,
  // so without the count a refused placement, a mirror's among them, was invisible).
  let refused = state
    .held_content
    .unauthorized()
    .saturating_sub(unauthorized_before);
  if refused > 0 {
    let count = state.refusals.entry(CONTENT_UNAUTHORIZED).or_insert(0);
    *count = count.saturating_add(refused);
  } else {
    let kind = if reply.is_empty() {
      CONTENT_ANSWERED_EMPTY
    } else {
      CONTENT_ANSWERED
    };
    let count = state.refusals.entry(kind).or_insert(0);
    *count = count.saturating_add(1);
  }
  let Some(object) = held else {
    return reply;
  };
  // A newer placement supersedes an older one still ahead of the records (AUD-29-43).
  crate::content_retention::retain_object(state, object);
  // The acknowledgement stands behind the shard's publish (AUD-29-59, §4.8 persistence before reply): only
  // content a committed image carries is acknowledged. A refused publish answers no acknowledgement; the
  // content stays held, charged, and rides the next publish that commits.
  if crate::verbs::publish_shard(state).is_err() {
    let count = state.refusals.entry(CONTENT_UNPUBLISHED).or_insert(0);
    *count = count.saturating_add(1);
    return Vec::new();
  }
  reply
}

/// The status count of content requests this holder's access rule refused (a placement from a peer that may not
/// place the object here, a read of an object the peer may not read).
pub(crate) const CONTENT_UNAUTHORIZED: &str = "fleet.content.unauthorized";

/// The status count of content requests this holder answered (an offer's missing set or acknowledgement, a chunk's
/// progress, a fetch's archive or piece).
pub(crate) const CONTENT_ANSWERED: &str = "fleet.content.answered";

/// The status count of content requests the access rule allowed but this holder answered empty: malformed,
/// unverifiable, past its capacity, refused by its retention rule, unstaged or unheld.
pub(crate) const CONTENT_ANSWERED_EMPTY: &str = "fleet.content.answered_empty";

/// The status count under which a holder records a put it held but could not publish into its shard's
/// recovery image, and so did not acknowledge (AUD-29-59).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
pub(crate) const CONTENT_UNPUBLISHED: &str = "fleet.content.unpublished";

/// One held register as a recovery image keeps it (§4.8 "persistence before reply"): its object and the acceptor's
/// durable state in the acceptor's one encoding (`PersistedAcceptor`: the owner and configuration generation it serves
/// under, the epoch its fence has seen, and the retired owner an install last moved its authority from, A-251). The
/// register's head piece carries it with no position, and each accepted position is a piece of its own
/// ([`HeldPosition`], A-367); recovery puts them back together.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct HeldRegister {
  object: [u8; 16],
  acceptor: PersistedAcceptor,
}

/// One accepted position of a held register as its own piece (A-367), keyed by its object and sequence: the epoch it was
/// accepted at and the record's bytes. A holder's publication carries the positions it accepted since its last one, so
/// an acknowledgement costs its record, never the ledger the holder already carries; before, the register was one piece,
/// and every version of a green re-published every version its holders kept.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct HeldPosition {
  epoch: u64,
  value: Vec<u8>,
}

/// Format: the kind byte of a held piece's key (A-270; `slates_vfs::recover::HeldPiece`): the mirror records, one held
/// register's head, one live copy's head, one live copy's entry, one live log this shard ships as its owner (A-344), one
/// held register's accepted position (A-367), and one stored chunk, one held manifest and one stage of the content hold
/// (A-376). Key order is kind, then object, then position. Kind 0, the content hold's whole image before layout 30, is
/// cut no more.
const KIND_MIRRORED: u8 = 1;
/// Format: see [`KIND_MIRRORED`].
const KIND_REGISTER: u8 = 2;
/// Format: see [`KIND_MIRRORED`].
const KIND_LIVE_HEAD: u8 = 3;
/// Format: see [`KIND_MIRRORED`].
const KIND_LIVE_ENTRY: u8 = 4;
/// Format: see [`KIND_MIRRORED`]; its bytes are a `live_ship::OwnedLive` (A-344).
const KIND_LIVE_OWNED: u8 = 5;
/// Format: see [`KIND_MIRRORED`]; its bytes are a [`HeldPosition`], its key's position the position's sequence (A-367).
const KIND_REGISTER_POSITION: u8 = 6;
/// Format: see [`KIND_MIRRORED`]; a stored chunk of the content hold (A-376), keyed by its identity's first 24 bytes (a
/// BLAKE3 output's prefix: a collision is a 2^96 search), its bytes the chunk's image (`ContentHold::piece_bytes`).
const KIND_CONTENT_CHUNK: u8 = 7;
/// Format: see [`KIND_MIRRORED`]; a manifest held for an object (A-376), keyed by the first 24 bytes of the BLAKE3 of its
/// object and identity (one manifest may be held for several objects), its bytes its identity and image.
const KIND_CONTENT_MANIFEST: u8 = 8;
/// Format: see [`KIND_MIRRORED`]; an object's stage of the content hold (A-376), keyed by its object, its bytes its image.
const KIND_CONTENT_STAGE: u8 = 9;
/// Format: the piece kind of a holder's durable checkpoint of a green's replica (A-404), keyed by the green.
const KIND_REPLICA_CHECKPOINT: u8 = 10;

/// Format: the piece kind of a repository window kept as a chunk of the shard's chunk store (A-727 slice a3), keyed by
/// its handle (`vcs_store::arena_windows`).
const KIND_VCS_WINDOW: u8 = 11;

/// Format: the piece kind of a list of the shard's repository content store (A-727 slice a3b), keyed by its identity's
/// first 24 bytes; its bytes the identity, then the list's.
const KIND_VCS_LIST: u8 = 12;

/// Format: the piece kind of an object's name in the shard's repository content store (A-727 slice a3b; the identity
/// rule), keyed by the identity's first 24 bytes; its bytes the identity (BLAKE3 of the object), then the root.
const KIND_VCS_NAME: u8 = 13;

/// The key of a repository content store's piece of `kind` for `identity`: its first 24 bytes, as a held chunk's is.
fn vcs_node_key(kind: u8, identity: &slates_vcs::ContentId) -> PieceKey {
  let object: [u8; 16] = identity.0[..16].try_into().unwrap_or_default();
  let position = identity.0[16..24].try_into().map_or(0, u64::from_be_bytes);
  piece_key(kind, object, position)
}

/// A list's piece bytes: its identity, then its bytes.
fn vcs_list_bytes(identity: &slates_vcs::ContentId, bytes: &[u8]) -> Vec<u8> {
  [identity.0.as_slice(), bytes].concat()
}

/// A name's piece bytes: the object's identity, then its root.
fn vcs_name_bytes(identity: &slates_vcs::ContentId, root: &slates_vcs::ContentId) -> Vec<u8> {
  [identity.0.as_slice(), root.0.as_slice()].concat()
}

/// A held piece's key: what [`slates_vfs::recover::HELD_PIECE_KEY_BYTES`] says.
type PieceKey = [u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES];

/// Format: where a piece key's object id starts, after its kind byte.
const KEY_OBJECT_AT: usize = size_of::<u8>();
/// Format: where a piece key's position starts, after its object id.
const KEY_POSITION_AT: usize = KEY_OBJECT_AT + size_of::<ObjectId>();

/// The key of the piece of `kind` for `object` at `position` (big-endian, so key order within an object is position
/// order).
fn piece_key(kind: u8, object: [u8; 16], position: u64) -> PieceKey {
  let mut key = [0u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES];
  for (byte, value) in key.iter_mut().zip(
    std::iter::once(kind)
      .chain(object)
      .chain(position.to_be_bytes()),
  ) {
    *byte = value;
  }
  key
}

/// The key of the content hold's `piece` (A-376): a chunk by its identity's first 24 bytes, a manifest by the first 24
/// bytes of the BLAKE3 of its object and identity, a stage by its object.
fn hold_piece_key(piece: &slates_cluster::content::HoldPiece) -> PieceKey {
  use slates_cluster::content::HoldPiece;
  let digest_key = |kind: u8, digest: &[u8; 32]| {
    let object_bytes = size_of::<ObjectId>();
    let object: [u8; 16] = digest
      .get(..object_bytes)
      .and_then(|bytes| bytes.try_into().ok())
      .unwrap_or_default();
    let position = digest
      .get(object_bytes..object_bytes.saturating_add(size_of::<u64>()))
      .and_then(|bytes| bytes.try_into().ok())
      .map_or(0, u64::from_be_bytes);
    piece_key(kind, object, position)
  };
  match piece {
    HoldPiece::Chunk(identity) => digest_key(KIND_CONTENT_CHUNK, identity),
    HoldPiece::Manifest(object, identity) => {
      let mut hasher = blake3::Hasher::new();
      hasher.update(&object.0);
      hasher.update(identity);
      digest_key(KIND_CONTENT_MANIFEST, hasher.finalize().as_bytes())
    }
    HoldPiece::Stage(object) => piece_key(KIND_CONTENT_STAGE, object.0, 0),
  }
}

/// A piece key's kind, object and position.
fn split_key(key: &PieceKey) -> (u8, [u8; 16], u64) {
  // A key always has its kind byte; `u8::MAX`, a kind no build cuts, is refused as unknown should one not.
  let kind = key.first().copied().unwrap_or(u8::MAX);
  let object = key
    .get(KEY_OBJECT_AT..KEY_POSITION_AT)
    .and_then(|bytes| bytes.try_into().ok())
    .unwrap_or_default();
  let position = key
    .get(KEY_POSITION_AT..)
    .and_then(|bytes| bytes.try_into().ok())
    .map_or(0, u64::from_be_bytes);
  (kind, object, position)
}

/// What a holder keeps for other owners, as its shard's recovery image carries it (A-270): the content it holds as a
/// piece per stored chunk, per held manifest and per stage (`ContentHold::pieces`, AUD-29-59, A-376), so a put's
/// publication carries the content it put; the mirror records, one whole piece; each register it holds as a head piece,
/// with its fence, and a piece per position it accepted (A-367), so an acknowledgement's publication carries the record
/// it acknowledged; and each copy of another owner's live log (A-200) as a head piece and a piece per entry, so a live
/// write's publication carries the entry it added. A warm restart keeps the member id, so all
/// of it must survive one: an acknowledged record a restarted holder no longer had would let its takeover promise count
/// toward `f + 1` without it (Vertical Paxos II's acceptor; Raft, Figure 2: persistent state before responding).
pub(crate) fn held_image(state: &ShardState) -> Vec<slates_vfs::recover::HeldPiece> {
  let mut pieces: Vec<slates_vfs::recover::HeldPiece> = whole_pieces(state)
    .into_iter()
    .map(|(key, bytes)| slates_vfs::recover::HeldPiece { key, bytes })
    .collect();
  pieces.extend(
    state
      .held_content
      .pieces()
      .into_iter()
      .map(|(piece, bytes)| slates_vfs::recover::HeldPiece {
        key: hold_piece_key(&piece),
        bytes,
      }),
  );
  for (object, acceptor) in &state.holder_records {
    pieces.push(register_head_piece(*object, acceptor));
    pieces.extend(
      acceptor
        .positions()
        .map(|(position, sequence, epoch, value)| {
          position_piece((position, sequence), epoch, value)
        }),
    );
  }
  for (object, held) in state.live.held.iter() {
    pieces.extend(live_pieces(*object, held, 0));
  }
  // Every live log this shard ships as its owner: its epoch, its next position and its fence (A-344).
  for (volume, owned) in crate::live_ship::owned_pieces(state) {
    pieces.push(owned_piece(volume, owned));
  }
  // Every durable checkpoint of a replica this holder keeps (A-404).
  for (object, held) in &state.merge.checkpoints {
    pieces.push(checkpoint_piece(*object, held));
  }
  // Every repository window kept as a chunk of the shard's chunk store, its block named (A-727 slice a3).
  if let Some(windows) = crate::vcs_store::chunk_windows(&state.vcs_stores) {
    pieces.extend(windows.pieces().into_iter().map(|(handle, bytes)| {
      slates_vfs::recover::HeldPiece {
        key: piece_key(KIND_VCS_WINDOW, [0; 16], handle),
        bytes,
      }
    }));
  }
  // The repository content store's lists and names, so an object past a window is whole after a restart (a3b).
  if let Some(content) = state.vcs_stores.content() {
    for (identity, bytes) in content.lists() {
      pieces.push(slates_vfs::recover::HeldPiece {
        key: vcs_node_key(KIND_VCS_LIST, &identity),
        bytes: vcs_list_bytes(&identity, bytes),
      });
    }
    for (identity, root) in content.names() {
      pieces.push(slates_vfs::recover::HeldPiece {
        key: vcs_node_key(KIND_VCS_NAME, &identity),
        bytes: vcs_name_bytes(&identity, &root),
      });
    }
  }
  pieces
}

/// A held checkpoint's piece (A-404), keyed by its green.
fn checkpoint_piece(
  object: ObjectId,
  held: &crate::merge_service::HeldCheckpoint,
) -> slates_vfs::recover::HeldPiece {
  slates_vfs::recover::HeldPiece {
    key: piece_key(KIND_REPLICA_CHECKPOINT, object.0, 0),
    bytes: held.to_piece(),
  }
}

/// One live log's kept piece (A-344): what this shard's owner side keeps of `volume`'s log across a restart.
fn owned_piece(
  volume: [u8; 16],
  owned: crate::live_ship::OwnedLive,
) -> slates_vfs::recover::HeldPiece {
  slates_vfs::recover::HeldPiece {
    key: piece_key(KIND_LIVE_OWNED, volume, 0),
    bytes: owned.to_bytes(),
  }
}

/// The pieces carried whole and compared by their bytes: the mirror records, only when not empty. (Owed: carried by
/// their own changes, as the registers, the live copies and, since A-376, the content hold are.)
fn whole_pieces(state: &ShardState) -> BTreeMap<PieceKey, Vec<u8>> {
  let mut pieces = BTreeMap::new();
  if !state.mirror_records.is_empty() {
    let mirrored: Vec<Vec<u8>> = state
      .mirror_records
      .iter()
      .map(|(_, record)| record.encode())
      .collect();
    pieces.insert(piece_key(KIND_MIRRORED, [0; 16], 0), mirrored.to_bytes());
  }
  pieces
}

/// One held register's head piece: its object and the acceptor's durable state but its positions, in the acceptor's one
/// encoding (`Acceptor::persisted_head`, A-251, A-367), its moved-from owner included.
fn register_head_piece(object: ObjectId, acceptor: &Acceptor) -> slates_vfs::recover::HeldPiece {
  let register = HeldRegister {
    object: object.0,
    acceptor: acceptor.persisted_head(),
  };
  slates_vfs::recover::HeldPiece {
    key: piece_key(KIND_REGISTER, object.0, 0),
    bytes: register.to_bytes(),
  }
}

/// One accepted position's piece (A-367), keyed by its object and sequence.
fn position_piece(
  (object, sequence): (ObjectId, u64),
  epoch: slates_db::register::HostEpoch,
  value: &[u8],
) -> slates_vfs::recover::HeldPiece {
  let position = HeldPosition {
    epoch: epoch.0,
    value: value.to_vec(),
  };
  slates_vfs::recover::HeldPiece {
    key: piece_key(KIND_REGISTER_POSITION, object.0, sequence),
    bytes: position.to_bytes(),
  }
}

/// A live copy's head piece and its entries' pieces from slot `from` on.
fn live_pieces(
  object: ObjectId,
  held: &crate::live_ship::Held,
  from: u64,
) -> Vec<slates_vfs::recover::HeldPiece> {
  let mut pieces = vec![slates_vfs::recover::HeldPiece {
    key: piece_key(KIND_LIVE_HEAD, object.0, 0),
    bytes: held.head_of(object).to_bytes(),
  }];
  let (first, end) = held.slots();
  for slot in first.max(from)..end {
    if let Some(entry) = held.entry_at(slot) {
      pieces.push(slates_vfs::recover::HeldPiece {
        key: piece_key(KIND_LIVE_ENTRY, object.0, slot),
        bytes: entry.to_vec(),
      });
    }
  }
  pieces
}

/// What of the held pieces the committed journal holds that the tracked maps cannot tell (A-270): the whole pieces'
/// bytes, compared at each publication; each live copy's published slots, so a delta drops the ones it no longer
/// holds and adds only the new ones; and each held register's published positions (A-367), so a register dropped whole
/// has each of its position pieces removed. The last is a sequence a position the journal carries, beside a position the
/// register itself keeps whole.
#[derive(Debug, Default)]
pub(crate) struct PublishedHeld {
  whole: BTreeMap<PieceKey, Vec<u8>>,
  live: BTreeMap<ObjectId, (u64, u64)>,
  registers: BTreeMap<ObjectId, std::collections::BTreeSet<(ObjectId, u64)>>,
}

/// The held pieces a delta carries (A-270): those put and those removed since the last committed publication, and the
/// whole pieces as they now are, to adopt once it commits ([`held_committed`]).
pub(crate) struct HeldChanges {
  /// The pieces put, each whole.
  pub(crate) put: Vec<slates_vfs::recover::HeldPiece>,
  /// The keys removed.
  pub(crate) removed: Vec<PieceKey>,
  /// The whole pieces as they now are, adopted once the delta commits.
  pub(crate) whole: BTreeMap<PieceKey, Vec<u8>>,
  /// The kept pieces of owned live logs it changes (A-344), kept once it commits (`live_ship::owned_committed`).
  pub(crate) owned: Vec<([u8; 16], Option<crate::live_ship::OwnedLive>)>,
}

/// What a delta carries of held register `object` (A-367): its head, and the positions its acceptor names written (put
/// while held, removed when not) and dropped (removed); or, dropped whole, its head and every position published. An
/// acceptor that keeps no changes (one made outside `fleet::hold_checked_record` and recovery, a test's), or whose
/// changes do not account for its positions against what was published (one put in place of another under the same
/// object), puts every position it holds and removes every published one it does not: the whole register, as its
/// pieces.
fn register_changes(
  state: &ShardState,
  object: ObjectId,
  (put, removed): (&mut Vec<slates_vfs::recover::HeldPiece>, &mut Vec<PieceKey>),
) {
  let published = state.published_held.registers.get(&object);
  let Some(acceptor) = state.holder_records.get(&object) else {
    removed.push(piece_key(KIND_REGISTER, object.0, 0));
    removed.extend(
      published
        .into_iter()
        .flatten()
        .map(|(position, sequence)| piece_key(KIND_REGISTER_POSITION, position.0, *sequence)),
    );
    return;
  };
  put.push(register_head_piece(object, acceptor));
  match acceptor
    .changes()
    .filter(|changes| changes_cover(acceptor, changes, published))
  {
    Some(changes) => {
      for &(position, sequence) in &changes.written {
        match acceptor.position(position, sequence) {
          Some((epoch, value)) => put.push(position_piece((position, sequence), epoch, value)),
          None => removed.push(piece_key(KIND_REGISTER_POSITION, position.0, sequence)),
        }
      }
      removed.extend(
        changes
          .dropped
          .iter()
          .map(|(position, sequence)| piece_key(KIND_REGISTER_POSITION, position.0, *sequence)),
      );
    }
    None => {
      put.extend(
        acceptor
          .positions()
          .map(|(position, sequence, epoch, value)| {
            position_piece((position, sequence), epoch, value)
          }),
      );
      removed.extend(
        published
          .into_iter()
          .flatten()
          .filter(|(position, sequence)| acceptor.position(*position, *sequence).is_none())
          .map(|(position, sequence)| piece_key(KIND_REGISTER_POSITION, position.0, *sequence)),
      );
    }
  }
}

/// Whether `changes` account for every position `acceptor` holds against what was `published` of it (A-367): the
/// published positions, with those written that were not published and without those dropped that were, number exactly
/// what it holds. An acceptor put in place of another under the same object fails it, since its changes name only its
/// own accepts, so its whole register is carried instead.
fn changes_cover(
  acceptor: &Acceptor,
  changes: &slates_db::register::PositionChanges,
  published: Option<&std::collections::BTreeSet<(ObjectId, u64)>>,
) -> bool {
  let published_count = published.map_or(0, std::collections::BTreeSet::len);
  let added = changes
    .written
    .iter()
    .filter(|position| !published.is_some_and(|published| published.contains(position)))
    .count();
  let gone = changes
    .dropped
    .iter()
    .filter(|position| published.is_some_and(|published| published.contains(position)))
    .count();
  published_count.saturating_add(added).checked_sub(gone) == Some(acceptor.position_count())
}

/// The content hold's pieces changed since the last committed publication (A-376): each held now put, each gone
/// removed.
fn content_hold_changes(
  state: &ShardState,
  (put, removed): (&mut Vec<slates_vfs::recover::HeldPiece>, &mut Vec<PieceKey>),
) {
  for piece in state.held_content.changed_pieces() {
    match state.held_content.piece_bytes(piece) {
      Some(bytes) => put.push(slates_vfs::recover::HeldPiece {
        key: hold_piece_key(piece),
        bytes,
      }),
      None => removed.push(hold_piece_key(piece)),
    }
  }
}

/// A live copy's changes since the last committed publication: its head and the entries in slots past the published
/// ones put, and the published slots it dropped removed; a copy let go removes its head and every published slot.
fn live_copy_changes(
  state: &ShardState,
  object: ObjectId,
  (put, removed): (&mut Vec<slates_vfs::recover::HeldPiece>, &mut Vec<PieceKey>),
) {
  let published = state.published_held.live.get(&object).copied();
  let Some(held) = state.live.held.get(&object) else {
    removed.push(piece_key(KIND_LIVE_HEAD, object.0, 0));
    if let Some((published_first, published_end)) = published {
      removed.extend(
        (published_first..published_end).map(|slot| piece_key(KIND_LIVE_ENTRY, object.0, slot)),
      );
    }
    return;
  };
  let (first, end) = held.slots();
  // A copy's slots only move forward, and a copy made again starts at or above the shard's slot floor, so every slot
  // below the published end that it still holds is the published entry.
  let (published_first, published_end) = published.unwrap_or((first, first));
  put.extend(live_pieces(object, held, published_end));
  removed.extend(
    (published_first..published_end.min(first))
      .chain(end.max(published_first)..published_end)
      .map(|slot| piece_key(KIND_LIVE_ENTRY, object.0, slot)),
  );
}

/// The replica checkpoints changed since the last committed publication (A-404): each kept now put, each dropped
/// removed.
fn checkpoint_changes(
  state: &ShardState,
  (put, removed): (&mut Vec<slates_vfs::recover::HeldPiece>, &mut Vec<PieceKey>),
) {
  for object in &state.merge.checkpoints_changed {
    match state.merge.checkpoints.get(object) {
      Some(held) => put.push(checkpoint_piece(*object, held)),
      None => removed.push(piece_key(KIND_REPLICA_CHECKPOINT, object.0, 0)),
    }
  }
}

/// The repository windows kept or let go since the last committed publication (A-727 slice a3): each kept put, each
/// let go removed.
fn vcs_window_changes(
  state: &ShardState,
  (put, removed): (&mut Vec<slates_vfs::recover::HeldPiece>, &mut Vec<PieceKey>),
) {
  if let Some(windows) = crate::vcs_store::chunk_windows(&state.vcs_stores) {
    for (handle, piece) in windows.changes() {
      let key = piece_key(KIND_VCS_WINDOW, [0; 16], handle);
      match piece {
        Some(bytes) => put.push(slates_vfs::recover::HeldPiece { key, bytes }),
        None => removed.push(key),
      }
    }
  }
}

/// The repository content store's lists and names kept or let go since then (a3b): each kept put, each let go removed.
fn vcs_node_changes(
  state: &ShardState,
  (put, removed): (&mut Vec<slates_vfs::recover::HeldPiece>, &mut Vec<PieceKey>),
) {
  if let Some(content) = state.vcs_stores.content() {
    let (lists, names) = content.changes();
    for (identity, bytes) in lists {
      let key = vcs_node_key(KIND_VCS_LIST, &identity);
      match bytes {
        Some(bytes) => put.push(slates_vfs::recover::HeldPiece {
          key,
          bytes: vcs_list_bytes(&identity, bytes),
        }),
        None => removed.push(key),
      }
    }
    for (identity, root) in names {
      let key = vcs_node_key(KIND_VCS_NAME, &identity);
      match root {
        Some(root) => put.push(slates_vfs::recover::HeldPiece {
          key,
          bytes: vcs_name_bytes(&identity, &root),
        }),
        None => removed.push(key),
      }
    }
  }
}

/// What a delta carries of the held pieces since the last committed publication (A-270): a whole piece whose bytes
/// differ; of every register its tracked map marked, its head and the positions its acceptor accepted or dropped since
/// (A-367), or, dropped whole, the head and every position published; and of a live copy, its head, the entries in slots
/// past the published ones, and the published slots it dropped. The work is the changes', not the holder's.
pub(crate) fn held_changes(state: &ShardState) -> HeldChanges {
  let whole = whole_pieces(state);
  let mut put = Vec::new();
  let mut removed = Vec::new();
  for (key, bytes) in &whole {
    if state.published_held.whole.get(key) != Some(bytes) {
      put.push(slates_vfs::recover::HeldPiece {
        key: *key,
        bytes: bytes.clone(),
      });
    }
  }
  removed.extend(
    state
      .published_held
      .whole
      .keys()
      .filter(|key| !whole.contains_key(*key))
      .copied(),
  );
  for object in state.holder_records.changed() {
    register_changes(state, *object, (&mut put, &mut removed));
  }
  // The content hold's pieces it changed since the last committed publication (A-376): each held now put, each gone
  // removed. The hold's whole image was re-encoded at every publication and re-put whenever it changed, which at f > 0
  // is every version of a green its holder takes.
  content_hold_changes(state, (&mut put, &mut removed));
  for object in state.live.held.changed() {
    live_copy_changes(state, *object, (&mut put, &mut removed));
  }
  // The replica checkpoints this holder changed since the last committed publication (A-404): each kept now put, each
  // dropped removed.
  checkpoint_changes(state, (&mut put, &mut removed));
  // The repository windows kept or let go since the last committed publication (A-727 slice a3).
  vcs_window_changes(state, (&mut put, &mut removed));
  // The repository content store's lists and names kept or let go since then (a3b).
  vcs_node_changes(state, (&mut put, &mut removed));
  // The live logs this shard ships whose kept piece this publication changes (A-344): each captured one at the position
  // its coming entry takes, and each marked; a log let go has its piece removed.
  let owned = crate::live_ship::owned_changes(state);
  for (volume, piece) in &owned {
    match piece {
      Some(piece) => put.push(owned_piece(*volume, *piece)),
      None => removed.push(piece_key(KIND_LIVE_OWNED, *volume, 0)),
    }
  }
  HeldChanges {
    put,
    removed,
    whole,
    owned,
  }
}

/// The kept pieces of owned live logs a checkpoint's held image carries (A-344), for `live_ship::owned_committed` once it
/// commits. A piece that does not decode is left out: the image was made here, so it never happens.
pub(crate) fn owned_of(
  pieces: &[slates_vfs::recover::HeldPiece],
) -> Vec<([u8; 16], Option<crate::live_ship::OwnedLive>)> {
  pieces
    .iter()
    .filter_map(|piece| {
      let (kind, volume, _) = split_key(&piece.key);
      (kind == KIND_LIVE_OWNED).then_some(())?;
      let owned = crate::live_ship::OwnedLive::from_bytes(&piece.bytes).ok()?;
      Some((volume, Some(owned)))
    })
    .collect()
}

/// Adopts what a committed publication carried of the held pieces (A-270): `whole` as the published whole pieces, each
/// live copy's slots as published (every copy's after a checkpoint, `everything`; the marked ones' after a delta), the
/// slot floor raised past them, and the tracked maps' marks cleared.
pub(crate) fn held_committed(
  state: &mut ShardState,
  whole: BTreeMap<PieceKey, Vec<u8>>,
  everything: bool,
) {
  state.published_held.whole = whole;
  if everything {
    state.published_held.live = state
      .live
      .held
      .iter()
      .map(|(object, held)| (*object, held.slots()))
      .collect();
  } else {
    let changed: Vec<ObjectId> = state.live.held.changed().iter().copied().collect();
    for object in changed {
      match state
        .live
        .held
        .get(&object)
        .map(crate::live_ship::Held::slots)
      {
        Some(slots) => {
          state.published_held.live.insert(object, slots);
        }
        None => {
          state.published_held.live.remove(&object);
        }
      }
    }
  }
  let floor = state
    .published_held
    .live
    .values()
    .map(|(_, end)| *end)
    .fold(state.live.slot_floor, u64::max);
  state.live.slot_floor = floor;
  registers_committed(state, everything);
  // What the publication carried of the content hold, its every change or, a checkpoint, all of it (A-376).
  state.held_content.clear_changes();
  state.holder_records.clear_changed();
  state.live.held.clear_changed();
  state.merge.checkpoints_changed.clear();
  if let Some(windows) = crate::vcs_store::chunk_windows_mut(&mut state.vcs_stores) {
    windows.clear_changes();
  }
  if let Some(content) = state.vcs_stores.content_mut() {
    content.clear_changes();
  }
  // The windows this publication carried are durable: every homes ticket kept before it answers so (A-724 with A-727 a3).
  crate::vcs_store::homes::publication_committed();
}

/// Adopts what a committed publication carried of the held registers (A-367): each register's published positions as
/// they now are — every register's after a checkpoint (`everything`), the marked ones' after a delta — and each marked
/// acceptor's changes forgotten, since the publication carried them. A delta is made and committed in one turn
/// (`verbs::publish_shard`), so nothing changes between what it carried and this.
fn registers_committed(state: &mut ShardState, everything: bool) {
  let objects: Vec<ObjectId> = if everything {
    state.published_held.registers.clear();
    state.holder_records.keys().copied().collect()
  } else {
    state.holder_records.changed().iter().copied().collect()
  };
  for object in objects {
    let Some(acceptor) = state.holder_records.get_mut(&object) else {
      state.published_held.registers.remove(&object);
      continue;
    };
    let covered = acceptor.changes().filter(|changes| {
      !everything
        && changes_cover(
          acceptor,
          changes,
          state.published_held.registers.get(&object),
        )
    });
    match covered {
      Some(changes) => {
        let published = state.published_held.registers.entry(object).or_default();
        for written in &changes.written {
          if acceptor.position(written.0, written.1).is_some() {
            published.insert(*written);
          } else {
            published.remove(written);
          }
        }
        for dropped in &changes.dropped {
          published.remove(dropped);
        }
      }
      None => {
        let positions = acceptor
          .positions()
          .map(|(position, sequence, ..)| (position, sequence))
          .collect();
        state.published_held.registers.insert(object, positions);
      }
    }
    acceptor.clear_changes();
  }
}

/// The whole pieces of a checkpoint about to stream, for [`held_committed`] once it commits.
pub(crate) fn whole_of(pieces: &[slates_vfs::recover::HeldPiece]) -> BTreeMap<PieceKey, Vec<u8>> {
  pieces
    .iter()
    .filter(|piece| split_key(&piece.key).0 == KIND_MIRRORED)
    .map(|piece| (piece.key, piece.bytes.clone()))
    .collect()
}

/// What a recovered image's held pieces say was published (A-270), so the journal's next delta is judged against it.
pub(crate) fn published_from(pieces: &[slates_vfs::recover::HeldPiece]) -> PublishedHeld {
  let mut live: BTreeMap<ObjectId, (u64, u64)> = BTreeMap::new();
  let mut registers: BTreeMap<ObjectId, std::collections::BTreeSet<(ObjectId, u64)>> =
    BTreeMap::new();
  for piece in pieces {
    let (kind, object, slot) = split_key(&piece.key);
    if kind == KIND_LIVE_ENTRY {
      let slots = live.entry(ObjectId(object)).or_insert((slot, slot));
      slots.0 = slots.0.min(slot);
      slots.1 = slots.1.max(slot.saturating_add(1));
    }
    // A held register's positions are its own object's (`fleet::hold_checked_record` accepts into the acceptor of the
    // record's object), so each published position is listed under its object (A-367).
    if kind == KIND_REGISTER_POSITION {
      registers
        .entry(ObjectId(object))
        .or_default()
        .insert((ObjectId(object), slot));
    }
  }
  PublishedHeld {
    whole: whole_of(pieces),
    live,
    registers,
  }
}

/// A recovered held image, split: the content hold's own image, and the held registers to rebuild.
pub(crate) struct RecoveredHeld {
  /// The content hold's image (`ContentHold::claim_image` reads it).
  pub(crate) content: Vec<u8>,
  registers: Vec<HeldRegister>,
  mirrored: Vec<Vec<u8>>,
  live: Vec<crate::live_ship::HeldLive>,
  /// The live logs this shard shipped as their owner, each kept piece by volume (A-344).
  owned: Vec<([u8; 16], crate::live_ship::OwnedLive)>,
  /// The durable checkpoints of replicas this holder kept, by green (A-404).
  checkpoints: Vec<(ObjectId, crate::merge_service::HeldCheckpoint)>,
  /// The repository windows kept as chunks of the shard's chunk store, each piece by handle (A-727 slice a3).
  pub(crate) windows: Vec<(u64, Vec<u8>)>,
  /// The repository content store's lists, each by identity with its bytes (a3b).
  pub(crate) lists: Vec<(slates_vcs::ContentId, Vec<u8>)>,
  /// The repository content store's names: an object's identity, then its root (a3b).
  pub(crate) names: Vec<(slates_vcs::ContentId, slates_vcs::ContentId)>,
}

/// Why a recovered image's held pieces were refused (A-270): never read as less than was held, since a holder that
/// cannot restore what it acknowledged must not serve as though it had.
#[derive(Debug)]
pub(crate) enum HeldRefused {
  /// A piece's bytes did not decode.
  Wire(slates_wire::WireError),
  /// A piece of a kind this build does not cut.
  UnknownKind(u8),
  /// A live entry with no head piece for its copy.
  EntryWithoutHead([u8; 16]),
  /// A live copy whose entries do not run on from its first slot without a gap.
  SlotGap([u8; 16]),
  /// A held register's position with no head piece for its register (A-367).
  PositionWithoutHead([u8; 16]),
  /// The content hold's pieces do not make its image (A-376).
  Content(slates_cluster::content::HoldImageError),
  /// A replica checkpoint's piece did not decode (A-404).
  Checkpoint([u8; 16]),
}

impl std::fmt::Display for HeldRefused {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      HeldRefused::Wire(error) => write!(f, "a held piece did not decode: {error:?}"),
      HeldRefused::UnknownKind(kind) => write!(f, "a held piece of unknown kind {kind}"),
      HeldRefused::EntryWithoutHead(object) => {
        write!(f, "a live entry of {object:02x?} with no head piece")
      }
      HeldRefused::SlotGap(object) => {
        write!(f, "the live copy of {object:02x?} has a gap in its entries")
      }
      HeldRefused::Content(refused) => {
        write!(
          f,
          "the content hold's pieces do not make its image: {refused:?}"
        )
      }
      HeldRefused::PositionWithoutHead(object) => {
        write!(
          f,
          "a held register position of {object:02x?} with no head piece"
        )
      }
      HeldRefused::Checkpoint(object) => {
        write!(f, "the replica checkpoint of {object:02x?} did not decode")
      }
    }
  }
}

impl From<slates_wire::WireError> for HeldRefused {
  fn from(error: slates_wire::WireError) -> Self {
    HeldRefused::Wire(error)
  }
}

/// A content hold's pieces by kind (A-376): its chunks', its manifests' and its stages' bytes.
type PiecesByKind = (Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Vec<u8>>);

/// Splits a recovered image's held pieces (A-270), each live copy joined from its head and its entries in slot order.
/// No pieces hold nothing; pieces that do not decode, or that leave a copy's entries short, are refused.
pub(crate) fn split_held(
  pieces: &[slates_vfs::recover::HeldPiece],
) -> Result<RecoveredHeld, HeldRefused> {
  let mut held = RecoveredHeld {
    content: Vec::new(),
    registers: Vec::new(),
    mirrored: Vec::new(),
    live: Vec::new(),
    owned: Vec::new(),
    checkpoints: Vec::new(),
    windows: Vec::new(),
    lists: Vec::new(),
    names: Vec::new(),
  };
  let mut heads: BTreeMap<[u8; 16], crate::live_ship::HeldLive> = BTreeMap::new();
  let mut entries: BTreeMap<[u8; 16], Vec<(u64, Vec<u8>)>> = BTreeMap::new();
  let mut positions: BTreeMap<[u8; 16], Vec<slates_db::register::PersistedPosition>> =
    BTreeMap::new();
  // The content hold's chunk, manifest and stage pieces (A-376), put back into its image once all are read.
  let mut content: PiecesByKind = Default::default();
  for piece in pieces {
    let (kind, object, slot) = split_key(&piece.key);
    match kind {
      KIND_CONTENT_CHUNK => content.0.push(piece.bytes.clone()),
      KIND_CONTENT_MANIFEST => content.1.push(piece.bytes.clone()),
      KIND_CONTENT_STAGE => content.2.push(piece.bytes.clone()),
      KIND_MIRRORED => held.mirrored = Vec::<Vec<u8>>::from_bytes(&piece.bytes)?,
      KIND_REGISTER => held.registers.push(HeldRegister::from_bytes(&piece.bytes)?),
      KIND_REGISTER_POSITION => {
        let position = HeldPosition::from_bytes(&piece.bytes)?;
        positions
          .entry(object)
          .or_default()
          .push(slates_db::register::PersistedPosition {
            object,
            sequence: slot,
            epoch: position.epoch,
            value: position.value,
          });
      }
      KIND_LIVE_HEAD => {
        heads.insert(
          object,
          crate::live_ship::HeldLive::from_bytes(&piece.bytes)?,
        );
      }
      KIND_LIVE_ENTRY => entries
        .entry(object)
        .or_default()
        .push((slot, piece.bytes.clone())),
      KIND_LIVE_OWNED => held.owned.push((
        object,
        crate::live_ship::OwnedLive::from_bytes(&piece.bytes)?,
      )),
      KIND_REPLICA_CHECKPOINT => held.checkpoints.push((
        ObjectId(object),
        crate::merge_service::HeldCheckpoint::from_piece(&piece.bytes)
          .ok_or(HeldRefused::Checkpoint(object))?,
      )),
      KIND_VCS_WINDOW => held.windows.push((slot, piece.bytes.clone())),
      KIND_VCS_LIST => {
        let (identity, bytes) = piece
          .bytes
          .split_at_checked(slates_vcs::content::IDENTITY_BYTES)
          .ok_or(HeldRefused::UnknownKind(kind))?;
        let identity =
          slates_vcs::ContentId::from_slice(identity).ok_or(HeldRefused::UnknownKind(kind))?;
        held.lists.push((identity, bytes.to_vec()));
      }
      KIND_VCS_NAME => {
        let (identity, root) = piece
          .bytes
          .split_at_checked(slates_vcs::content::IDENTITY_BYTES)
          .ok_or(HeldRefused::UnknownKind(kind))?;
        let identity =
          slates_vcs::ContentId::from_slice(identity).ok_or(HeldRefused::UnknownKind(kind))?;
        let root = slates_vcs::ContentId::from_slice(root).ok_or(HeldRefused::UnknownKind(kind))?;
        held.names.push((identity, root));
      }
      other => return Err(HeldRefused::UnknownKind(other)),
    }
  }
  join_positions(&mut held.registers, positions)?;
  held.content =
    slates_cluster::content::ContentHold::image_from_pieces(&content.0, &content.1, &content.2)
      .map_err(HeldRefused::Content)?;
  if let Some(object) = entries.keys().find(|object| !heads.contains_key(*object)) {
    return Err(HeldRefused::EntryWithoutHead(*object));
  }
  for (object, head) in heads {
    let copy_entries = entries.remove(&object).unwrap_or_default();
    let runs_on = copy_entries.iter().enumerate().all(|(index, (slot, _))| {
      u64::try_from(index)
        .ok()
        .and_then(|index| head.first().checked_add(index))
        == Some(*slot)
    });
    if !runs_on || head.object() != object {
      return Err(HeldRefused::SlotGap(object));
    }
    held
      .live
      .push(head.with_entries(copy_entries.into_iter().map(|(_, bytes)| bytes).collect()));
  }
  Ok(held)
}

/// Puts each held register's positions back beside its head (A-367), in sequence order, as `Acceptor::to_persisted`
/// lists them; a position whose register has no head piece is refused, since the image would hold an accepted record
/// with no authority or fence to read it under.
fn join_positions(
  registers: &mut [HeldRegister],
  mut positions: BTreeMap<[u8; 16], Vec<slates_db::register::PersistedPosition>>,
) -> Result<(), HeldRefused> {
  for register in registers.iter_mut() {
    let mut own = positions.remove(&register.object).unwrap_or_default();
    own.sort_by_key(|position| position.sequence);
    register.acceptor.accepted = own;
  }
  match positions.keys().next() {
    Some(object) => Err(HeldRefused::PositionWithoutHead(*object)),
    None => Ok(()),
  }
}

/// Rebuilds the held registers of a recovered image as `local`, each the acceptor it persisted
/// (`Acceptor::from_persisted`: its fence, accepted positions and moved-from owner); the count rebuilt.
pub(crate) fn restore_held_registers(
  state: &mut ShardState,
  local: HostId,
  mut held: RecoveredHeld,
) -> usize {
  let count = held.registers.len();
  crate::live_ship::restore_copies(state, std::mem::take(&mut held.live));
  crate::live_ship::restore_owned(state, std::mem::take(&mut held.owned));
  // The replica checkpoints (A-404): the image carries each, so none is owed to the next publication.
  state.merge.checkpoints = std::mem::take(&mut held.checkpoints).into_iter().collect();
  state.merge.checkpoints_changed.clear();
  // The mirror records, each the newest of its object as it was published. One that does not decode, or that the
  // register refuses, is counted: the image held it, and this holder no longer does.
  for bytes in &held.mirrored {
    let restored = slates_db::register::Record::decode(bytes)
      .ok()
      .is_some_and(|record| state.mirror_records.accept(record).is_ok());
    if !restored {
      state.count(MIRROR_RESTORE_REFUSED, 1);
    }
  }
  for register in held.registers {
    let object = ObjectId(register.object);
    // Kept changes from here on (A-367): the image it was rebuilt from carries each position it holds as its own piece,
    // so none is owed, and the next publication carries the ones it accepts or drops.
    let mut acceptor = Acceptor::from_persisted(local, register.acceptor).tracking_changes();
    acceptor.clear_changes();
    let owner = acceptor.owner();
    state.holder_records.insert(object, acceptor);
    // The routing view is not persisted, and every install reaches a held object only through it: the takeover's
    // resolve moves it to its successor, the authority reconcile raises it to the installed generation, and a page
    // lists and fences it. So the restored copy is routed to the owner its acceptor names, as accepting a record does.
    // Unrouted, a holder restarted warm answered a successor's page empty, leaving its copy unfenced, and refused the
    // successor's adoption `ForeignGeneration` every period
    // (`a_restored_holders_copy_moves_to_its_successor_and_accepts_its_adoption`).
    state.fleet.track_object_owner(object, owner);
  }
  count
}

/// The status count of mirror records a recovered held image carried that could not be held again.
/// Format: a refusal name in the daemon's status report.
const MIRROR_RESTORE_REFUSED: &str = "fleet.mirror.restore_refused";
