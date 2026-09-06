//! The authority a shared-I/O request carries and the owner-side records that make it enforceable
//! (§4.8, §4.13; the inode-addressed-io design). A request identifies its object by [`ObjectId`]
//! and rides an [`Attachment`] — an owned, generation-checked record that binds a volume, a view,
//! an enrolled subject, the granted rights and the owner epoch it was admitted under. The seam
//! builds an [`OpContext`] from that validated record — never from anything a caller declares —
//! and refuses when the authority cannot be established.
//!
//! This is the *local* slice of §4.8/§4.13 that makes the context enforceable, not a placeholder:
//! `f = 0` (a laptop) is the same protocol with one owner epoch, and a revoked attachment or a
//! stale epoch is refused here exactly as a fenced holder is refused in a fleet — the fleet adds
//! membership, takeover and cross-region, not a second implementation and not a relaxed check.

use slates_db::catalog::{Principal, VolumeId};
use slates_mem::{Handle, Slab};

use crate::VfsError;

/// Shape: the bound on concurrently live attachments per owner — more than any realistic number of
/// mounts and exports at once, few enough that the registry is a small table; a runaway is a typed
/// `MemError::SlabFull` refusal. Charging attachments against §4.2 admission is owed.
const MAX_ATTACHMENTS: usize = 4096;
/// Shape: the attachment slab's segment size (a page of slots), so the table grows a page at a time.
const ATTACHMENT_SEGMENT: usize = 256;
/// Format: the first owner epoch. Zero is reserved as "no epoch", so a live owner starts at one and
/// a takeover raises it (§4.8).
const FIRST_EPOCH: u64 = 1;

/// The object a request addresses (§4.6): its inode number and a stable generation. Identity
/// survives copy-on-write; the generation distinguishes a re-minted number across incarnations (§6
/// of the design), and is a stable value within one incarnation because inode numbers are never
/// reused (D-4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectId {
  /// The inode number.
  pub inode: u64,
  /// The generation.
  pub generation: u64,
}

impl ObjectId {
  /// The object named by inode number `inode` at generation `generation`. A transport that does
  /// not track generations (FUSE addresses by node id) passes zero, the live generation until
  /// generation-tracked reuse lands (§4.6 `(no, gen)`); NFS carries the handle's encoded value.
  pub fn new(inode: u64, generation: u64) -> ObjectId {
    ObjectId { inode, generation }
  }
}

/// Which state a request sees: the volume's current head, or a pinned immutable version (§4.16 an
/// `advance` re-pins). A write against a pinned view is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
  /// The volume's current head.
  Current,
  /// A pinned immutable version (a green-volume or snapshot attachment).
  Version(u64),
}

/// The access an attachment was granted at attach time. Rights are derived from the attachment and
/// enforced before an effect; a caller never declares its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rights {
  /// May read.
  pub read: bool,
  /// May write.
  pub write: bool,
}

/// Whether an attachment is still usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachmentState {
  /// Admitted and not revoked.
  Live,
  /// Revoked; its records must be preserved for in-flight drain but it admits no new effect.
  Revoked,
}

/// An owned, generation-checked attachment record. A request rides one; the seam validates it
/// before any effect.
struct Attachment {
  volume: VolumeId,
  view: View,
  subject: Principal,
  rights: Rights,
  epoch: u64,
  state: AttachmentState,
}

/// A handle to an attachment. It is generation-checked by the registry, so a revoked or reused
/// slot is refused; only [`Attachments::attach`] mints one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentId(Handle<Attachment>);

impl AttachmentId {
  /// A stable per-process key for this attachment: its slab index and generation packed into one
  /// word. The volume core attributes references to an attachment by this opaque `u64`
  /// ([`crate::VfsError`]-free), so a teardown sweep releases exactly this attachment's references.
  /// It is process-local, not the durable §4.8 attachment id; reconciling the two is owed with the
  /// server wiring.
  pub fn key(self) -> u64 {
    (u64::from(self.0.index()) << u32::BITS) | u64::from(self.0.generation())
  }
}

/// The authenticated attachment/view context a request carries, constructed by the owner from a
/// validated [`Attachment`] — never caller-declared. Read/write consult it for the view and the
/// granted access; the transport edges carry it per request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpContext {
  /// The attachment this context was built from (for the request-lifetime pin).
  pub attachment: AttachmentId,
  /// The volume the attachment binds; an object addressed under this context must belong to it.
  pub volume: VolumeId,
  /// The view resolved for the operation.
  pub view: View,
  /// The enrolled subject the attachment was admitted for.
  pub subject: Principal,
  /// The access the attachment was granted.
  pub rights: Rights,
  /// The owner epoch the attachment was admitted under (checked against the current epoch).
  pub epoch: u64,
}

/// The owner's attachment registry: it admits attachments under the current host epoch, validates
/// them before effects, and revokes and fences them. The single-owner shard writes it (§4.8's
/// register model, `f = 0` degenerate); every check here is real — a revoked or stale-epoch
/// attachment is refused, never waved through.
pub struct Attachments {
  registry: Slab<Attachment>,
  epoch: u64,
}

impl Default for Attachments {
  fn default() -> Attachments {
    Attachments::new()
  }
}

impl Attachments {
  /// An empty registry at the first owner epoch.
  pub fn new() -> Attachments {
    Attachments {
      registry: Slab::new(ATTACHMENT_SEGMENT, MAX_ATTACHMENTS),
      epoch: FIRST_EPOCH,
    }
  }

  /// The current owner epoch.
  pub fn epoch(&self) -> u64 {
    self.epoch
  }

  /// Admits an attachment for the enrolled `subject` on `volume`/`view` with `rights`, under the
  /// current owner epoch. The `subject` is established by trusted enrollment (the daemon's
  /// authenticated connection), not declared per request. Refuses at the registry bound.
  pub fn attach(
    &mut self,
    volume: VolumeId,
    view: View,
    subject: Principal,
    rights: Rights,
  ) -> Result<AttachmentId, VfsError> {
    let epoch = self.epoch;
    let handle = self.registry.insert(Attachment {
      volume,
      view,
      subject,
      rights,
      epoch,
      state: AttachmentState::Live,
    })?;
    Ok(AttachmentId(handle))
  }

  /// Marks an attachment revoked: it admits no new effect (a later [`Attachments::context`] on it
  /// refuses), while its record survives for an in-flight drain. `drain` frees the slot once the
  /// last in-flight request has released it.
  pub fn revoke(&mut self, id: AttachmentId) {
    if let Ok(attachment) = self.registry.get_mut(id.0) {
      attachment.state = AttachmentState::Revoked;
    }
  }

  /// Frees a revoked attachment's slot once its in-flight requests have drained; its handle's
  /// generation moves on, so any lingering reference is refused. A live attachment is not drained.
  pub fn drain(&mut self, id: AttachmentId) {
    if matches!(
      self.registry.get(id.0).map(|a| a.state),
      Ok(AttachmentState::Revoked)
    ) {
      let _ = self.registry.remove(id.0);
    }
  }

  /// Raises the owner epoch (a takeover, §4.8). Attachments admitted under the old epoch are fenced
  /// — a later [`Attachments::context`] on them refuses — until they are re-admitted. At `f = 0`
  /// this is driven by recovery, not by fleet membership, but it is the same check.
  pub fn take_over(&mut self) {
    self.epoch = self.epoch.saturating_add(1);
  }

  /// Builds the validated context for an operation on `id`, or refuses (`NotPermitted`) when the
  /// attachment is unknown, revoked, or admitted under a superseded epoch — the "adapter cannot
  /// establish the required authority" case. The context is constructed here from the record.
  pub fn context(&self, id: AttachmentId) -> Result<OpContext, VfsError> {
    let attachment = self
      .registry
      .get(id.0)
      .map_err(|_| VfsError::NotPermitted)?;
    if attachment.state != AttachmentState::Live {
      return Err(VfsError::NotPermitted);
    }
    if attachment.epoch != self.epoch {
      return Err(VfsError::NotPermitted);
    }
    Ok(OpContext {
      attachment: id,
      volume: attachment.volume,
      view: attachment.view,
      subject: attachment.subject.clone(),
      rights: attachment.rights,
      epoch: attachment.epoch,
    })
  }
}
