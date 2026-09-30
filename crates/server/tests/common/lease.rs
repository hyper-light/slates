//! A target's landing lease as a test holds it (§4.15 step 4; AUD-29-03): the key the daemon holds a
//! target's lease under, and a hold and a release on the daemon's control shard — the one owner of the
//! host's target leases — standing for another landing attempt.

use std::path::Path;

use slates_ipc::protocol::Refusal;
use slates_land::grant::{LandingLease, TargetIdentity, lease_key};
use slates_land::os::OsLand;
use slates_server::Daemon;
use slates_server::daemon::OBSERVE_BUDGET_NS;
use slates_server::landing::{release_target_lease, take_target_lease};
use slates_vfs::host::HostFs;

/// The key the daemon holds `path`'s lease under: the canonical identity the server derives, the opened
/// directory's device and inode.
pub(crate) fn lease_key_of(path: &str) -> String {
  let (mut os, target) = OsLand::open_target(Path::new(path)).unwrap();
  let identity = os.fingerprint_dir(target.dir).unwrap();
  lease_key(&TargetIdentity {
    key: target.key.clone(),
    device: identity.dev,
    inode: identity.ino,
  })
}

/// Takes the lease on `key` on `daemon`'s control shard for the attempt `holder`, for `term_ns`: the
/// control shard's answer.
pub(crate) fn hold(
  daemon: &Daemon,
  key: &str,
  holder: u64,
  term_ns: u64,
) -> Result<LandingLease, Refusal> {
  let key = key.to_owned();
  let deadline = slates_machine::clock::monotonic_ns().saturating_add(term_ns);
  daemon
    .observe_control(OBSERVE_BUDGET_NS, move |s| {
      take_target_lease(s, key.clone(), holder, term_ns, deadline)
    })
    .expect("the control shard answers")
}

/// Releases the lease on `key` on `daemon`'s control shard when the attempt `holder` holds it.
pub(crate) fn release(daemon: &Daemon, key: &str, holder: u64) {
  let key = key.to_owned();
  daemon
    .observe_control(OBSERVE_BUDGET_NS, move |s| {
      release_target_lease(s, &key, holder)
    })
    .expect("the control shard answers")
    .expect("the release is recorded");
}
