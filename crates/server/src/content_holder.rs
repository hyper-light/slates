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
  Acceptor, Authority, HostEpoch, HostId, ObjectId, RegionId, RegionalConfiguration,
  RootConfiguration,
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

/// One held register as a recovery image keeps it (§4.8 "persistence before reply"): the owner and configuration
/// generation it serves under, the epoch its fence has seen, and every position it accepted.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct HeldRegister {
  object: [u8; 16],
  owner: u64,
  generation: u64,
  promised: u64,
  accepted: Vec<HeldPosition>,
}

/// One accepted position of a held register.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct HeldPosition {
  object: [u8; 16],
  sequence: u64,
  epoch: u64,
  value: Vec<u8>,
}

/// Format: the kind byte of a held piece's key (A-260; `slates_vfs::recover::HeldPiece`): the content hold's image, the
/// mirror records, one held register, one live copy's head, and one live copy's entry. Key order is kind, then object,
/// then position.
const KIND_CONTENT: u8 = 0;
/// Format: see [`KIND_CONTENT`].
const KIND_MIRRORED: u8 = 1;
/// Format: see [`KIND_CONTENT`].
const KIND_REGISTER: u8 = 2;
/// Format: see [`KIND_CONTENT`].
const KIND_LIVE_HEAD: u8 = 3;
/// Format: see [`KIND_CONTENT`].
const KIND_LIVE_ENTRY: u8 = 4;

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

/// A piece key's kind, object and position.
fn split_key(key: &PieceKey) -> (u8, [u8; 16], u64) {
  let kind = key.first().copied().unwrap_or(KIND_CONTENT);
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

/// What a holder keeps for other owners, as its shard's recovery image carries it (A-260): the content it holds
/// (`ContentHold`'s own image, AUD-29-59) and the mirror records, each one whole piece; each register record it
/// acknowledged, with its fence, one piece; and each copy of another owner's live log (A-200) as a head piece and a
/// piece per entry, so a live write's publication carries the entry it added. A warm restart keeps the member id, so all
/// of it must survive one: an acknowledged record a restarted holder no longer had would let its takeover promise count
/// toward `f + 1` without it (Vertical Paxos II's acceptor; Raft, Figure 2: persistent state before responding).
pub(crate) fn held_image(state: &ShardState) -> Vec<slates_vfs::recover::HeldPiece> {
  let mut pieces: Vec<slates_vfs::recover::HeldPiece> = whole_pieces(state)
    .into_iter()
    .map(|(key, bytes)| slates_vfs::recover::HeldPiece { key, bytes })
    .collect();
  for (object, acceptor) in &state.holder_records {
    pieces.push(register_piece(*object, acceptor));
  }
  for (object, held) in state.live.held.iter() {
    pieces.extend(live_pieces(*object, held, 0));
  }
  pieces
}

/// The pieces carried whole and compared by their bytes: the content hold's image and the mirror records, each only
/// when not empty. (Owed: each carried by its own changes, as the registers and live copies are.)
fn whole_pieces(state: &ShardState) -> BTreeMap<PieceKey, Vec<u8>> {
  let mut pieces = BTreeMap::new();
  let content = state.held_content.to_image();
  if !content.is_empty() {
    pieces.insert(piece_key(KIND_CONTENT, [0; 16], 0), content);
  }
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

/// One held register's piece: its owner, generation, fence and every position it accepted.
fn register_piece(object: ObjectId, acceptor: &Acceptor) -> slates_vfs::recover::HeldPiece {
  let (promised, accepted) = acceptor.persisted();
  let register = HeldRegister {
    object: object.0,
    owner: acceptor.owner().0,
    generation: acceptor.generation(),
    promised: promised.0,
    accepted: accepted
      .into_iter()
      .map(|(object, sequence, epoch, value)| HeldPosition {
        object: object.0,
        sequence,
        epoch: epoch.0,
        value,
      })
      .collect(),
  };
  slates_vfs::recover::HeldPiece {
    key: piece_key(KIND_REGISTER, object.0, 0),
    bytes: register.to_bytes(),
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

/// What of the held pieces the committed journal holds that the tracked maps cannot tell (A-260): the whole pieces'
/// bytes, compared at each publication, and each live copy's published slots, so a delta drops the ones it no longer
/// holds and adds only the new ones.
#[derive(Debug, Default)]
pub(crate) struct PublishedHeld {
  whole: BTreeMap<PieceKey, Vec<u8>>,
  live: BTreeMap<ObjectId, (u64, u64)>,
}

/// The held pieces a delta carries (A-260): those put and those removed since the last committed publication, and the
/// whole pieces as they now are, to adopt once it commits ([`held_committed`]).
pub(crate) struct HeldChanges {
  /// The pieces put, each whole.
  pub(crate) put: Vec<slates_vfs::recover::HeldPiece>,
  /// The keys removed.
  pub(crate) removed: Vec<PieceKey>,
  /// The whole pieces as they now are, adopted once the delta commits.
  pub(crate) whole: BTreeMap<PieceKey, Vec<u8>>,
}

/// What a delta carries of the held pieces since the last committed publication (A-260): a whole piece whose bytes
/// differ; every register and live copy its tracked map marked, put when held and removed when not; and of a live copy,
/// its head, the entries in slots past the published ones, and the published slots it dropped. The work is the changes',
/// not the holder's.
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
    match state.holder_records.get(object) {
      Some(acceptor) => put.push(register_piece(*object, acceptor)),
      None => removed.push(piece_key(KIND_REGISTER, object.0, 0)),
    }
  }
  for object in state.live.held.changed() {
    let published = state.published_held.live.get(object).copied();
    match state.live.held.get(object) {
      Some(held) => {
        let (first, end) = held.slots();
        // A copy's slots only move forward, and a copy made again starts at or above the shard's slot floor, so every
        // slot below the published end that it still holds is the published entry.
        let (published_first, published_end) = published.unwrap_or((first, first));
        put.extend(live_pieces(*object, held, published_end));
        removed.extend(
          (published_first..published_end.min(first))
            .chain(end.max(published_first)..published_end)
            .map(|slot| piece_key(KIND_LIVE_ENTRY, object.0, slot)),
        );
      }
      None => {
        removed.push(piece_key(KIND_LIVE_HEAD, object.0, 0));
        if let Some((published_first, published_end)) = published {
          removed.extend(
            (published_first..published_end).map(|slot| piece_key(KIND_LIVE_ENTRY, object.0, slot)),
          );
        }
      }
    }
  }
  HeldChanges {
    put,
    removed,
    whole,
  }
}

/// Adopts what a committed publication carried of the held pieces (A-260): `whole` as the published whole pieces, each
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
  state.holder_records.clear_changed();
  state.live.held.clear_changed();
}

/// The whole pieces of a checkpoint about to stream, for [`held_committed`] once it commits.
pub(crate) fn whole_of(pieces: &[slates_vfs::recover::HeldPiece]) -> BTreeMap<PieceKey, Vec<u8>> {
  pieces
    .iter()
    .filter(|piece| matches!(split_key(&piece.key).0, KIND_CONTENT | KIND_MIRRORED))
    .map(|piece| (piece.key, piece.bytes.clone()))
    .collect()
}

/// What a recovered image's held pieces say was published (A-260), so the journal's next delta is judged against it.
pub(crate) fn published_from(pieces: &[slates_vfs::recover::HeldPiece]) -> PublishedHeld {
  let mut live: BTreeMap<ObjectId, (u64, u64)> = BTreeMap::new();
  for piece in pieces {
    let (kind, object, slot) = split_key(&piece.key);
    if kind == KIND_LIVE_ENTRY {
      let slots = live.entry(ObjectId(object)).or_insert((slot, slot));
      slots.0 = slots.0.min(slot);
      slots.1 = slots.1.max(slot.saturating_add(1));
    }
  }
  PublishedHeld {
    whole: whole_of(pieces),
    live,
  }
}

/// A recovered held image, split: the content hold's own image, and the held registers to rebuild.
pub(crate) struct RecoveredHeld {
  /// The content hold's image (`ContentHold::claim_image` reads it).
  pub(crate) content: Vec<u8>,
  registers: Vec<HeldRegister>,
  mirrored: Vec<Vec<u8>>,
  live: Vec<crate::live_ship::HeldLive>,
}

/// Why a recovered image's held pieces were refused (A-260): never read as less than was held, since a holder that
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
    }
  }
}

impl From<slates_wire::WireError> for HeldRefused {
  fn from(error: slates_wire::WireError) -> Self {
    HeldRefused::Wire(error)
  }
}

/// Splits a recovered image's held pieces (A-260), each live copy joined from its head and its entries in slot order.
/// No pieces hold nothing; pieces that do not decode, or that leave a copy's entries short, are refused.
pub(crate) fn split_held(
  pieces: &[slates_vfs::recover::HeldPiece],
) -> Result<RecoveredHeld, HeldRefused> {
  let mut held = RecoveredHeld {
    content: Vec::new(),
    registers: Vec::new(),
    mirrored: Vec::new(),
    live: Vec::new(),
  };
  let mut heads: BTreeMap<[u8; 16], crate::live_ship::HeldLive> = BTreeMap::new();
  let mut entries: BTreeMap<[u8; 16], Vec<(u64, Vec<u8>)>> = BTreeMap::new();
  for piece in pieces {
    let (kind, object, slot) = split_key(&piece.key);
    match kind {
      KIND_CONTENT => held.content.clone_from(&piece.bytes),
      KIND_MIRRORED => held.mirrored = Vec::<Vec<u8>>::from_bytes(&piece.bytes)?,
      KIND_REGISTER => held.registers.push(HeldRegister::from_bytes(&piece.bytes)?),
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
      other => return Err(HeldRefused::UnknownKind(other)),
    }
  }
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

/// Rebuilds the held registers of a recovered image as `local`, each with its fence and accepted positions
/// (`Acceptor::recovered`); the count rebuilt.
pub(crate) fn restore_held_registers(
  state: &mut ShardState,
  local: HostId,
  mut held: RecoveredHeld,
) -> usize {
  let count = held.registers.len();
  crate::live_ship::restore_copies(state, std::mem::take(&mut held.live));
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
    let accepted = register
      .accepted
      .into_iter()
      .map(|position| {
        (
          ObjectId(position.object),
          position.sequence,
          HostEpoch(position.epoch),
          position.value,
        )
      })
      .collect();
    let (object, owner) = (ObjectId(register.object), HostId(register.owner));
    let acceptor = Acceptor::recovered(
      local,
      Authority {
        generation: register.generation,
        owner,
      },
      HostEpoch(register.promised),
      accepted,
    );
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
