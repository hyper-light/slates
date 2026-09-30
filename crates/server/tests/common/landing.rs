//! A daemon's landing flow as a test drives it (§4.15): a client connected with the product's own deadlines,
//! scratch volumes, and a landing presented and approved the way the human's surface approves it (the proof
//! under the daemon's issuer secret).

use std::time::{Duration, Instant};

use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, Landing, NamePolicy, SizeClass, SnapshotId, VolumeId,
};
use slates_ipc::protocol::{Filter, GrantScope};

/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);
/// Shape: the grant term of the tests' approvals: a minute, far past any test.
pub(crate) const GRANT_TERM_NS: u64 = 60_000_000_000;

/// The product's own deadlines (`Deadlines::derive` over the anchor's liveness budget and the recovery
/// budget).
pub(crate) fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

/// A client of `instance`, retrying the rendezvous while the daemon starts.
pub(crate) fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        std::hint::spin_loop();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

/// A one-mebibyte scratch volume named `name`.
pub(crate) fn scratch(name: &str) -> CreateSpec {
  CreateSpec {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

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
