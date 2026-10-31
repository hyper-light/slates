//! A daemon's landing flow as a test drives it (§4.15): a client connected with the product's own deadlines,
//! scratch volumes, and a landing presented and approved the way the human's surface approves it (the proof
//! under the daemon's issuer secret).

use slates_client::{Client, Landing, SnapshotId, VolumeId};
use slates_ipc::protocol::{Filter, GrantScope};
/// Shape: the grant term of the tests' approvals: a minute, far past any test.
pub(crate) const GRANT_TERM_NS: u64 = 60_000_000_000;

/// Presents the landing of `snapshot` (the head when `None`) into `target` and approves it once; the
/// grant id.
pub(crate) fn approve(
  client: &mut Client,
  secret: &[u8; 32],
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  target: &str,
) -> u64 {
  let presented = client.land(volume, snapshot, target, Filter::default(), None);
  let Ok(Landing::GrantRequired {
    landing, manifest, ..
  }) = presented
  else {
    panic!("the landing was not presented: {presented:?}");
  };
  let proof = slates_server::landing::grant_proof(
    secret,
    landing,
    &manifest,
    GrantScope::Once,
    GRANT_TERM_NS,
  );
  client
    .grant(landing, manifest, GrantScope::Once, GRANT_TERM_NS, proof)
    .expect("the approval issues a grant")
}
