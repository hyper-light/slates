//! The node's sealing root (A-92 piece 2a; hyper-raft `docs/seal.md` §3, §8): the key every tenant key on this node is
//! wrapped under, held for the anchor's life and never written to a disk (R1).
//!
//! At boot the daemon locks hyper-seal's key region (one process-wide region of locked pages, out of core dumps on
//! Linux, sized once), locks the anchor's supervision page the same way, and then either finds the root a previous
//! daemon under this anchor published there, or makes one from the secure random source and publishes it. So a daemon
//! restart keeps the root (and with it every tenant key, and so every volume's sealed content), and the root lives
//! exactly as long as the RAM it protects: the anchor's. Each shard then reads the root from its own attachment of the
//! segment into a key of its own in the locked region ([`shard_root`]), as it reads the grant-issuer secret: no key
//! crosses a thread, and no global holds one, so two daemons in one process (the fleet tests) each have their own.
//!
//! Where the OS will not lock the key region or the anchor's page, sealing is unavailable: reported in status, never
//! used unlocked, and the daemon still serves (`RootState::Unavailable`).

use hyper_seal::Secret32;
use hyper_seal::keys::{KeyId, WrappingKey};
use slates_anchor::AnchorSegment;
use slates_anchor::layout::{SEAL_ROOT_ID_BYTES, SEAL_ROOT_KEY_BYTES};

/// Format: the generation a node root is made at; a rotation (a new root wrapping the tenant keys again, seal.md
/// §3.1) is a later generation.
const ROOT_GENERATION: u32 = 1;

/// How this daemon came by the node's sealing root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootState {
  /// Sealing is unavailable: the key region or the anchor's page could not be locked, or the random source refused.
  Unavailable,
  /// Made by this daemon and published into the anchor's locked page, for every daemon after it.
  Minted,
  /// Found in the anchor's page, made by a previous daemon under this anchor.
  Adopted,
}

impl RootState {
  /// The state's name in a status report.
  pub fn name(self) -> &'static str {
    match self {
      RootState::Unavailable => "unavailable",
      RootState::Minted => "minted",
      RootState::Adopted => "adopted",
    }
  }
}

/// Locks the key region for `slots` keys and the anchor's page, then finds or makes the node's root in `segment` (the
/// module doc). The calling thread holds no key afterwards; the shards read the root themselves.
pub fn init(segment: &mut AnchorSegment, slots: usize) -> RootState {
  // The process's region is made once; a later daemon in the same process (an in-process fleet, a restarted
  // daemon's tests) is answered by the region already made: enough for its keys, or `AlreadyLocked` with the count it
  // holds — never a smaller region taken as enough.
  match hyper_seal::lock_keys(slots) {
    Ok(()) => {}
    Err(hyper_seal::SealError::AlreadyLocked { slots: held }) => {
      eprintln!(
        "slates-server: sealing unavailable: this process's key region holds {held} keys, fewer than the {slots} \
         this daemon needs"
      );
      return RootState::Unavailable;
    }
    Err(e) => {
      eprintln!(
        "slates-server: sealing unavailable: a key region of {slots} keys could not be locked: {e}"
      );
      return RootState::Unavailable;
    }
  }
  if segment.protect_seal_page().is_err() {
    eprintln!("slates-server: sealing unavailable: the anchor's key page could not be locked");
    return RootState::Unavailable;
  }
  if matches!(segment.seal_root(), Ok(Some(_))) {
    return RootState::Adopted;
  }
  let (mut id, mut key) = ([0u8; SEAL_ROOT_ID_BYTES], [0u8; SEAL_ROOT_KEY_BYTES]);
  let made = slates_transport::handshake::secure_random(&mut id).is_ok()
    && slates_transport::handshake::secure_random(&mut key).is_ok();
  let published = made && segment.publish_seal_root(&id, &key).is_ok();
  key.fill(0);
  if published {
    RootState::Minted
  } else {
    eprintln!("slates-server: sealing unavailable: the root could not be made or published");
    RootState::Unavailable
  }
}

/// A shard's own copy of the node's root, read from its attachment of the anchor's segment into the locked region:
/// `None` while sealing is unavailable (`state`), or when the page holds no root or the region is full.
pub fn shard_root(segment: &AnchorSegment, state: RootState) -> Option<WrappingKey> {
  if state == RootState::Unavailable {
    return None;
  }
  let (id, mut key) = segment.seal_root().ok()??;
  let secret = Secret32::from_bytes(&key);
  key.fill(0);
  Some(WrappingKey::new(KeyId(id), ROOT_GENERATION, secret.ok()?))
}

/// A refusal to reach a tenant's or a volume's sealing key (A-92 piece 2b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealKeyError {
  /// Sealing is unavailable on this node (no root: the module doc).
  Unavailable,
  /// hyper-seal refused: a record that will not unwrap under its parent (another root's, or altered), a full key
  /// region, or the random source.
  Seal(hyper_seal::SealError),
  /// A new key's record could not be made durable; the partition was rolled back and the key is not used.
  Record,
}

impl From<hyper_seal::SealError> for SealKeyError {
  fn from(error: hyper_seal::SealError) -> SealKeyError {
    SealKeyError::Seal(error)
  }
}

/// Format: the generation tenant, naming and lineage keys are made at (a rotation is a later one, seal.md §3.1).
const KEY_GENERATION: u32 = 1;

/// The tenant key of `account` on this partition, unwrapped from its record under the node's root, or made and
/// recorded now. A tenant (the host account, A-9) has one key per partition, so a shard never asks another for it
/// (D-14); erasing a tenant destroys each partition's record.
pub fn tenant(
  state: &mut crate::state::ShardState,
  account: u64,
) -> Result<WrappingKey, SealKeyError> {
  let owner = slates_db::catalog::SealKeyOwner::Tenant { account };
  let recorded = state.db.partition().seal_key(&owner).cloned();
  let root = state.seal_root.as_ref().ok_or(SealKeyError::Unavailable)?;
  match recorded {
    Some(record) => open(root, &record),
    None => {
      let (key, record) = make(root, owner)?;
      commit(state, record)?;
      Ok(key)
    }
  }
}

/// The naming key of `volume` (seal.md §7: keyed names for its chunks), owned by `account`: a child of the volume's
/// lineage key, recorded wrapped under it, so a successor that adopts the lineage key verifies the names the dead owner
/// made (A-92 piece 4c).
pub fn namer(
  state: &mut crate::state::ShardState,
  volume: slates_db::catalog::VolumeId,
  account: u64,
) -> Result<hyper_seal::name::Namer, SealKeyError> {
  let lineage_key = lineage(state, volume, account)?;
  let owner = slates_db::catalog::SealKeyOwner::Naming { volume };
  let secret = match state.db.partition().seal_key(&owner).cloned() {
    Some(record) => lineage_key.unwrap(&hyper_seal::keys::Wrapped::decode(&record.record)?)?,
    None => {
      let (secret, wrapped) = lineage_key.make_child()?;
      let id = KeyId::random()?;
      commit(state, record_of(owner, id, &wrapped))?;
      secret
    }
  };
  Ok(hyper_seal::name::Namer::new(&secret)?)
}

/// The lineage key of `volume`, owned by `account`, under its tenant key: what seals the volume's chunks for holders.
pub fn lineage(
  state: &mut crate::state::ShardState,
  volume: slates_db::catalog::VolumeId,
  account: u64,
) -> Result<WrappingKey, SealKeyError> {
  let tenant_key = tenant(state, account)?;
  let owner = slates_db::catalog::SealKeyOwner::Lineage { volume };
  match state.db.partition().seal_key(&owner).cloned() {
    Some(record) => open(&tenant_key, &record),
    None => {
      let (key, record) = make(&tenant_key, owner)?;
      commit(state, record)?;
      Ok(key)
    }
  }
}

/// The key `record` holds, unwrapped under `parent`.
fn open(
  parent: &WrappingKey,
  record: &slates_db::catalog::SealKeyRecord,
) -> Result<WrappingKey, SealKeyError> {
  let secret = parent.unwrap(&hyper_seal::keys::Wrapped::decode(&record.record)?)?;
  Ok(WrappingKey::new(KeyId(record.id), KEY_GENERATION, secret))
}

/// A new key for `owner`, a child of `parent`, and its record.
fn make(
  parent: &WrappingKey,
  owner: slates_db::catalog::SealKeyOwner,
) -> Result<(WrappingKey, slates_db::catalog::SealKeyRecord), SealKeyError> {
  let (secret, wrapped) = parent.make_child()?;
  let id = KeyId::random()?;
  Ok((
    WrappingKey::new(id, KEY_GENERATION, secret),
    record_of(owner, id, &wrapped),
  ))
}

/// The record of `owner`'s key `id`, wrapped as `wrapped`.
fn record_of(
  owner: slates_db::catalog::SealKeyOwner,
  id: KeyId,
  wrapped: &hyper_seal::keys::Wrapped,
) -> slates_db::catalog::SealKeyRecord {
  slates_db::catalog::SealKeyRecord {
    owner,
    id: id.0,
    record: wrapped.encode().to_vec(),
  }
}

/// Commits `record` as one transaction of the shard's partition: the key is used only once its record is durable, so
/// nothing is ever sealed under a key a restart could not unwrap again.
fn commit(
  state: &mut crate::state::ShardState,
  record: slates_db::catalog::SealKeyRecord,
) -> Result<(), SealKeyError> {
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
  state.db.begin();
  let applied = state
    .db
    .mutate(
      &mut state.segment,
      &slates_db::Op::SealKeySet { record },
      now,
    )
    .is_ok();
  let committed = state.db.commit(&mut state.segment).is_ok();
  if applied && committed {
    Ok(())
  } else {
    Err(SealKeyError::Record)
  }
}

/// The node's ML-KEM-1024 recipient (A-92 piece 4a; seal.md §6): opened from its record in partition 0 under the node's
/// root, or made now (hybrid, with P-384) and recorded sealed under the root. It lives as long as the root, so keys
/// wrapped to it stay openable across a daemon restart and die with the anchor. Called on the control shard, whose
/// partition holds the record.
pub fn recipient(
  state: &mut crate::state::ShardState,
) -> Result<hyper_seal::recipient::Recipient, SealKeyError> {
  let owner = slates_db::catalog::SealKeyOwner::Recipient;
  let recorded = state.db.partition().seal_key(&owner).cloned();
  let root = state.seal_root.as_ref().ok_or(SealKeyError::Unavailable)?;
  if let Some(record) = recorded {
    return Ok(hyper_seal::recipient::Recipient::open(
      root,
      &record.record,
    )?);
  }
  let made = hyper_seal::recipient::Recipient::generate(true)?;
  let sealed = made.seal(root)?;
  commit(
    state,
    slates_db::catalog::SealKeyRecord {
      owner,
      id: made.public().id,
      record: sealed,
    },
  )?;
  Ok(made)
}

/// Format: a recipient public key on the wire (A-92 piece 4b): its id (16), its ML-KEM-1024 encapsulation key
/// (1,568), then `1` and its P-384 public point (97) when it takes hybrid records, or `0`.
const PUBLIC_HYBRID: u8 = 1;
/// Format: the flag of a recipient that takes CNSA records only.
const PUBLIC_CNSA: u8 = 0;
/// Format: a recipient's id (hyper-seal's random 128-bit id).
const RECIPIENT_ID_BYTES: usize = 16;

/// `public` as the recipient stream carries it.
pub fn encode_public(public: &hyper_seal::recipient::RecipientPublic) -> Vec<u8> {
  let mut out = Vec::with_capacity(
    RECIPIENT_ID_BYTES
      + hyper_seal::recipient::KEM_PUBLIC
      + size_of::<u8>()
      + hyper_seal::recipient::ECDH_PUBLIC,
  );
  out.extend_from_slice(&public.id);
  out.extend_from_slice(&public.kem);
  match &public.ecdh {
    Some(point) => {
      out.push(PUBLIC_HYBRID);
      out.extend_from_slice(point);
    }
    None => out.push(PUBLIC_CNSA),
  }
  out
}

/// The recipient public key `bytes` carries, refusing any other length or flag (hostile input, §4.9).
pub fn decode_public(bytes: &[u8]) -> Option<hyper_seal::recipient::RecipientPublic> {
  let (id, rest) = bytes.split_at_checked(RECIPIENT_ID_BYTES)?;
  let (kem, rest) = rest.split_at_checked(hyper_seal::recipient::KEM_PUBLIC)?;
  let (&flag, rest) = rest.split_first()?;
  let ecdh = match (flag, rest.len()) {
    (PUBLIC_HYBRID, len) if len == hyper_seal::recipient::ECDH_PUBLIC => {
      Some(rest.try_into().ok()?)
    }
    (PUBLIC_CNSA, 0) => None,
    _ => return None,
  };
  Some(hyper_seal::recipient::RecipientPublic {
    id: id.try_into().ok()?,
    kem: kem.to_vec(),
    ecdh,
  })
}

/// This node's recipient public key as the recipient stream answers it, on the control shard; empty while sealing is
/// unavailable (the peer then pairs nothing with this node).
pub fn node_recipient_public(state: &crate::state::ShardState) -> Vec<u8> {
  state
    .seal_recipient
    .as_ref()
    .map(|recipient| encode_public(&recipient.public()))
    .unwrap_or_default()
}

/// The pair key this owner shard keeps with candidate `host`, made and recorded under the root if new, wrapped to the
/// candidate's `public` key for delivery (A-92 piece 4b): its id and the hybrid ML-KEM record.
pub fn pair_for_delivery(
  state: &mut crate::state::ShardState,
  host: u64,
  public: &hyper_seal::recipient::RecipientPublic,
) -> Result<([u8; 16], Vec<u8>), SealKeyError> {
  let owner = slates_db::catalog::SealKeyOwner::Pair {
    host,
    partition: state.partition,
  };
  let recorded = state.db.partition().seal_key(&owner).cloned();
  let root = state.seal_root.as_ref().ok_or(SealKeyError::Unavailable)?;
  let (id, secret, fresh) = match recorded {
    Some(record) => (
      record.id,
      root.unwrap(&hyper_seal::keys::Wrapped::decode(&record.record)?)?,
      None,
    ),
    None => {
      let (secret, wrapped) = root.make_child()?;
      let id = KeyId::random()?;
      (id.0, secret, Some(record_of(owner, id, &wrapped)))
    }
  };
  let delivery = hyper_seal::recipient::wrap_to(
    public,
    hyper_seal::recipient::Mode::Hybrid,
    KeyId(id),
    &secret,
  )?;
  if let Some(record) = fresh {
    commit(state, record)?;
  }
  Ok((id, delivery))
}

/// A pair key an owner's shard `partition` on `host` delivered (A-92 piece 4b), on the control shard: unwrapped with this
/// node's recipient and recorded under its root. A second delivery of the recorded key is accepted; another key for
/// a recorded pair is refused, as the partition refuses a second record for an owner.
pub fn accept_pair(
  state: &mut crate::state::ShardState,
  (host, partition): (u64, u16),
  id: [u8; 16],
  delivery: &[u8],
) -> Result<(), SealKeyError> {
  let owner = slates_db::catalog::SealKeyOwner::Pair { host, partition };
  if let Some(record) = state.db.partition().seal_key(&owner) {
    return if record.id == id {
      Ok(())
    } else {
      Err(SealKeyError::Record)
    };
  }
  let recipient = state
    .seal_recipient
    .as_ref()
    .ok_or(SealKeyError::Unavailable)?;
  let secret = recipient.unwrap(KeyId(id), delivery)?;
  let root = state.seal_root.as_ref().ok_or(SealKeyError::Unavailable)?;
  let wrapped = root.wrap(&secret)?;
  commit(state, record_of(owner, KeyId(id), &wrapped))
}

/// The pair key recorded for `owner` here, unwrapped under the root: on an owner shard its key with a candidate, on a
/// candidate's control shard an owner shard's key with it.
pub fn pair_key(
  state: &crate::state::ShardState,
  owner: slates_db::catalog::SealKeyOwner,
) -> Result<WrappingKey, SealKeyError> {
  let record = state
    .db
    .partition()
    .seal_key(&owner)
    .cloned()
    .ok_or(SealKeyError::Record)?;
  let root = state.seal_root.as_ref().ok_or(SealKeyError::Unavailable)?;
  open(root, &record)
}

/// The tenant a volume owned by `principal` seals under (A-9: the host account): the database's own rule
/// ([`slates_db::catalog::Principal::tenant`]), which a volume's destroy also reads to erase a tenant's last key.
pub fn tenant_of(principal: &slates_db::catalog::Principal) -> u64 {
  principal.tenant()
}

/// The lineage key of `volume` as recorded, unwrapped read-only (no key is made): the tenant's record under the root,
/// then the volume's under the tenant. `None` while either is unrecorded or sealing is unavailable.
fn recorded_lineage_secret(
  state: &crate::state::ShardState,
  volume: slates_db::catalog::VolumeId,
  account: u64,
) -> Option<(KeyId, Secret32)> {
  use slates_db::catalog::SealKeyOwner;
  let root = state.seal_root.as_ref()?;
  let tenant_record = state
    .db
    .partition()
    .seal_key(&SealKeyOwner::Tenant { account })?;
  let tenant_key = open(root, tenant_record).ok()?;
  let lineage_record = state
    .db
    .partition()
    .seal_key(&SealKeyOwner::Lineage { volume })?;
  let secret = tenant_key
    .unwrap(&hyper_seal::keys::Wrapped::decode(&lineage_record.record).ok()?)
    .ok()?;
  Some((KeyId(lineage_record.id), secret))
}

/// What a sealed head carries for successors (A-92 piece 4c): the volume's lineage key wrapped under the pair key this
/// owner shard delivered to each neighbour, in anchor order (deterministic for one set of delivered pairs). `None` while
/// the volume has no recorded lineage key or this shard has delivered no pair.
pub fn head_sealing(
  state: &crate::state::ShardState,
  volume: slates_db::catalog::VolumeId,
  owner: &slates_db::catalog::Principal,
) -> Option<crate::head::HeadSealing> {
  let (lineage, secret) = recorded_lineage_secret(state, volume, tenant_of(owner))?;
  let keys: Vec<crate::head::HeadKey> = state
    .pairs_delivered
    .iter()
    .filter_map(|anchor| {
      let pair = pair_key(
        state,
        slates_db::catalog::SealKeyOwner::Pair {
          host: *anchor,
          partition: state.partition,
        },
      )
      .ok()?;
      Some(crate::head::HeadKey {
        anchor: *anchor,
        wrapped: pair.wrap(&secret).ok()?.encode().to_vec(),
      })
    })
    .collect();
  if keys.is_empty() {
    return None;
  }
  let naming = state
    .db
    .partition()
    .seal_key(&slates_db::catalog::SealKeyOwner::Naming { volume })
    .map(|record| crate::head::HeadNaming {
      id: record.id,
      wrapped: record.record.clone(),
    });
  Some(crate::head::HeadSealing {
    owner_anchor: state.origin_anchor.0,
    partition: state.partition,
    lineage: lineage.0,
    keys,
    naming,
  })
}

/// A successor's lineage key for a taken-over volume (A-92 piece 4c), on its control shard: its own entry in the head's
/// `sealing`, unwrapped under the pair key the owner's shard delivered to this node, then wrapped under this node's root
/// for the owner shard of the volume here (a 61-byte record, never the key's bytes, crosses the shards). `None` when the
/// head names no entry for this node (it acknowledged the head before its pair arrived) or the pair is unrecorded.
pub fn successor_lineage(
  state: &crate::state::ShardState,
  sealing: &crate::head::HeadSealing,
) -> Option<([u8; 16], Vec<u8>)> {
  let own = sealing
    .keys
    .iter()
    .find(|key| key.anchor == state.origin_anchor.0)?;
  let pair = pair_key(
    state,
    slates_db::catalog::SealKeyOwner::Pair {
      host: sealing.owner_anchor,
      partition: sealing.partition,
    },
  )
  .ok()?;
  let secret = pair
    .unwrap(&hyper_seal::keys::Wrapped::decode(&own.wrapped).ok()?)
    .ok()?;
  let root = state.seal_root.as_ref()?;
  Some((sealing.lineage, root.wrap(&secret).ok()?.encode().to_vec()))
}

/// Records a taken-over volume's lineage key on its new owner shard (A-92 piece 4c): `wrapped` (the key under this node's
/// root, from [`successor_lineage`]) unwrapped and recorded under the volume's tenant here, with the key's own id, so
/// the successor seals under the same key the dead owner did. A lineage already recorded here is kept.
pub fn adopt_lineage(
  state: &mut crate::state::ShardState,
  volume: slates_db::catalog::VolumeId,
  owner: &slates_db::catalog::Principal,
  (id, wrapped): ([u8; 16], Vec<u8>),
  naming: Option<&crate::head::HeadNaming>,
) -> Result<(), SealKeyError> {
  let owner_key = slates_db::catalog::SealKeyOwner::Lineage { volume };
  if state.db.partition().seal_key(&owner_key).is_none() {
    let tenant_key = tenant(state, tenant_of(owner))?;
    let root = state.seal_root.as_ref().ok_or(SealKeyError::Unavailable)?;
    let secret = root.unwrap(&hyper_seal::keys::Wrapped::decode(&wrapped)?)?;
    let rewrapped = tenant_key.wrap(&secret)?;
    commit(state, record_of(owner_key, KeyId(id), &rewrapped))?;
  }
  // The volume's naming key travels as the owner recorded it: wrapped under the lineage key both nodes now hold.
  let naming_key = slates_db::catalog::SealKeyOwner::Naming { volume };
  if let Some(naming) = naming
    && state.db.partition().seal_key(&naming_key).is_none()
  {
    commit(
      state,
      slates_db::catalog::SealKeyRecord {
        owner: naming_key,
        id: naming.id,
        record: naming.wrapped.clone(),
      },
    )?;
  }
  Ok(())
}
