//! Lists a reply at a time (§4.3: a reply is one bulk chunk; A-263 to A-267): every list the daemon can hold more of
//! than one reply carries — the volumes, a principal's grants, the audit, the paths a green changed, an overlay's
//! drift — is answered a page at a time. A consumer that shows a list a screen at a time asks for one [`Page`] a
//! screen and holds no more than a page; the whole-list verbs ([`Instance::list`], [`Instance::grants`],
//! [`Instance::audit`], [`Instance::changed_since`], [`Instance::drift`]) walk the pages for a consumer that wants every
//! item.
//!
//! **What a walk promises.** Each list's order is a key that never changes for its items: a volume's id, a grant's id,
//! a record's sequence, a green version and the paths it changed. A page continues past the last key the previous page
//! held, so an item that stands for the whole walk is answered exactly once however the list changes meanwhile; one
//! made or removed during the walk may or may not be. An overlay's drift continues from a position, as a directory
//! listing does, so a drift that changes between pages may repeat or skip an entry. A page whose cursor would not move
//! the walk forward is refused [`Error::Protocol`] ([`crate::ProtocolFault::UnexpectedReply`], a fault in slates), so a
//! walk always ends.
//!
//! **Why not one reply.** Until 2026-11-08 each of these lists was one reply, refused "reply too large for the bulk
//! area" past 100 volumes on a two-shard daemon, 800 changed files, 50 to 100 audit records, 25 to 50 grants and 600
//! drifted entries (`docs/bugs/2026-11-08-*`): an operator could not list what a server holds.

use slates_ipc::protocol::{ChangedCursor, VolumeId, VolumeSummary};

use crate::error::{Error, Missing};
use crate::instance::Instance;
use crate::observe::{AuditRecord, GrantRecord, audit_of, grant_of};
use crate::volume::{DriftPage, drift_page_of};

/// One page of a list (see the module doc): the items in the list's order, and the cursor the next page is asked
/// from, `None` when this page ends the list. The daemon's own type, as the client hands it on.
pub use slates_client::Page;

impl Instance {
  /// One page of the caller's volumes (§4.4 `list`, A-263): in volume-id order past `after` (`None` begins), as many as
  /// one reply holds, and the volume the next page is asked after. [`Instance::list`] walks them all.
  pub fn list_page(
    &mut self,
    after: Option<VolumeId>,
  ) -> Result<Page<VolumeSummary, VolumeId>, Error> {
    self.client.list_page(after).map_err(|e| self.error(e))
  }

  /// One page of the caller's grants (§4.13, A-266), typed: in grant-id order past `after` (`None` begins), as many as
  /// one reply holds, and the grant the next page is asked after. A grant leaves the list at its terminal step (used,
  /// revoked, past its term); its history is the audit. [`Instance::grants`] walks them all.
  pub fn grants_page(&mut self, after: Option<u64>) -> Result<Page<GrantRecord, u64>, Error> {
    let page = self.client.grants_page(after).map_err(|e| self.error(e))?;
    Ok(Page {
      items: page
        .items
        .into_iter()
        .map(grant_of)
        .collect::<Result<_, _>>()?,
      next: page.next,
    })
  }

  /// One page of the audit (§4.15, §4.14; A-265) from record `since`, typed, in sequence order, as many as one reply holds,
  /// and the sequence the next page is asked from. One partition numbers the daemon's audit and keeps a bounded window
  /// of the newest records, so a `since` older than the window starts at its oldest. [`Instance::audit`] walks them
  /// all.
  pub fn audit_page(&mut self, since: u64) -> Result<Page<AuditRecord, u64>, Error> {
    let page = self.client.audit_page(since).map_err(|e| self.error(e))?;
    Ok(Page {
      items: page
        .items
        .into_iter()
        .map(audit_of)
        .collect::<Result<_, _>>()?,
      next: page.next,
    })
  }

  /// The paths a green changed strictly after `version`, in any dimension — content, links, directories, modes,
  /// extended attributes, special files (§4.16 "Attachments and versions", A-264): how a work volume learns which of
  /// its pending paths the green moved under, as awareness and never as a guard (the merge verdict decides). In path
  /// order, each once, walked a page at a time. A version past the green's head is [`Error::Stale`].
  pub fn changed_since(&mut self, green: VolumeId, version: u64) -> Result<Vec<String>, Error> {
    self
      .client
      .changed_since(green, version)
      .map_err(|e| self.fail(e, Some(Missing::Volume(green))))
  }

  /// One page of [`Instance::changed_since`] from `from` (`None` begins): read from the green's change index in version
  /// order, so a page costs the changes it passes and never the green's tree. A page may hold no path and still carry
  /// a cursor (it stopped where the daemon's slice of a turn was spent); a path changed when the walk began is
  /// answered once, and again if it changes during the walk.
  pub fn changed_since_page(
    &mut self,
    green: VolumeId,
    version: u64,
    from: Option<ChangedCursor>,
  ) -> Result<Page<String, ChangedCursor>, Error> {
    self
      .client
      .changed_since_page(green, version, from)
      .map_err(|e| self.fail(e, Some(Missing::Volume(green))))
  }

  /// One page of an overlay's drift (§4.15 "Drift", A-267) from position `from` (0 begins), typed as a status types
  /// it, with the position the next page is read from. A status carries the first drifted entries that fit beside its
  /// other fields and counts the rest ([`crate::DriftReport::elided`]); this reads them all. Each page re-checks the
  /// witnessed entries against the disk, as a status does.
  pub fn drift_page(&mut self, volume: VolumeId, from: u64) -> Result<DriftPage, Error> {
    let page = self
      .client
      .drifted_page(volume, from)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))?;
    Ok(drift_page_of(page))
  }
}
