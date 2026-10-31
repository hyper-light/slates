//! A client of a test's daemon as the product makes one, on every platform: connected with the product's own deadlines,
//! and scratch volumes. The landing fixtures (`landing.rs`, Unix while the daemon lands nothing on Windows) build on
//! these; the NFS write-back suite, which the Windows server answers too (A-184, A-192), uses them directly.

use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, CreateSpec, Deadlines, NamePolicy, SizeClass};

/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);

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

/// A four-mebibyte scratch volume named `name`.
pub(crate) fn scratch(name: &str) -> CreateSpec {
  CreateSpec {
    name: name.to_owned(),
    // 4 MiB: room for the fairness test's 600 files in the inode allowance, a quota's share of the slab at about
    // 2.4 KB per inode (A-109); 1 MiB allowed 436.
    size: SizeClass::Bounded { limit: 4 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}
