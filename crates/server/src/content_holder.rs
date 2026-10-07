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
  let (council, records) = (&state.council, &state.holder_records);
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
  let (reply, held) = state.held_content.serve(
    &mut hold_space(&mut state.store),
    local,
    request,
    |access, object| authorized(council.configuration(), records, &mirror, access, object),
    |object, sequence, manifest| {
      crate::content_retention::admits(records, local, object, sequence, manifest)
    },
  );
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

/// What a holder keeps for other owners in its shard's recovery image: the content it holds (`ContentHold`'s own
/// image, AUD-29-59) and the register records it acknowledged, each with its fence. A warm restart keeps the member
/// id, so both must survive it: an acknowledged record a restarted holder no longer had would let its takeover
/// promise count toward `f + 1` without it (Vertical Paxos II's acceptor; Raft, Figure 2: persistent state before
/// responding).
#[derive(Clone, Debug, Default, PartialEq, Eq, Wire)]
struct HeldImage {
  content: Vec<u8>,
  registers: Vec<HeldRegister>,
}

/// The bytes of everything this shard holds for other owners, for its recovery image: empty when it holds nothing,
/// so a laptop's image is unchanged.
pub(crate) fn held_image(state: &ShardState) -> Vec<u8> {
  let content = state.held_content.to_image();
  if content.is_empty() && state.holder_records.is_empty() {
    return Vec::new();
  }
  let registers = state
    .holder_records
    .iter()
    .map(|(object, acceptor)| {
      let (promised, accepted) = acceptor.persisted();
      HeldRegister {
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
      }
    })
    .collect();
  HeldImage { content, registers }.to_bytes()
}

/// A recovered held image, split: the content hold's own image, and the held registers to rebuild.
pub(crate) struct RecoveredHeld {
  /// The content hold's image (`ContentHold::claim_image` reads it).
  pub(crate) content: Vec<u8>,
  registers: Vec<HeldRegister>,
}

/// Splits a recovered held image. Empty bytes hold nothing; bytes that do not decode are refused, never read as
/// nothing held: a holder that cannot restore what it acknowledged must not serve as though it had.
pub(crate) fn split_held(bytes: &[u8]) -> Result<RecoveredHeld, slates_wire::WireError> {
  if bytes.is_empty() {
    return Ok(RecoveredHeld {
      content: Vec::new(),
      registers: Vec::new(),
    });
  }
  let image = HeldImage::from_bytes(bytes)?;
  Ok(RecoveredHeld {
    content: image.content,
    registers: image.registers,
  })
}

/// Rebuilds the held registers of a recovered image as `local`, each with its fence and accepted positions
/// (`Acceptor::recovered`); the count rebuilt.
pub(crate) fn restore_held_registers(
  state: &mut ShardState,
  local: HostId,
  held: RecoveredHeld,
) -> usize {
  let count = held.registers.len();
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
    let acceptor = Acceptor::recovered(
      local,
      Authority {
        generation: register.generation,
        owner: HostId(register.owner),
      },
      HostEpoch(register.promised),
      accepted,
    );
    state
      .holder_records
      .insert(ObjectId(register.object), acceptor);
  }
  count
}
