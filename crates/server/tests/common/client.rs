//! A client of a test's daemon as the product makes one, on every platform: connected with the product's own deadlines,
//! scratch volumes, and the host account the daemon authenticated a client's channel as. The landing fixtures
//! (`landing.rs`, on every platform since the Windows writer, A-185) build on these; the NFS write-back suite, which the
//! Windows server answers too (A-184, A-192), uses them directly.

use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, CreateSpec, Deadlines, NamePolicy, SizeClass};

/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);
/// Shape: how long a test waits for a shard of its daemon to take a client it connected, as `attachment_term.rs` waits.
const TAKEN_WAIT: Duration = Duration::from_secs(20);

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

/// The host account the daemon authenticated `client_id`'s channel as, asked of each shard until one has taken the
/// client: the uid a consumer of that account is enrolled under, on every platform (a Windows channel's account is the
/// daemon's mapping of its token, never `getuid`). As `consumers.rs` and `attachment_term.rs` read it.
pub(crate) fn channel_account(daemon: &slates_server::Daemon, client_id: u32) -> u32 {
  let started = Instant::now();
  loop {
    let found = daemon.shards().iter().copied().find_map(|shard| {
      daemon
        .observation(
          Some(shard),
          slates_server::daemon::OBSERVE_BUDGET_NS,
          move |state: &mut slates_server::state::ShardState| {
            state
              .clients
              .iter()
              .find(|(_, slot)| slot.client_id == client_id)
              .map(|(_, slot)| slot.principal.clone())
          },
        )
        .unwrap()
        .wait()
        .unwrap()
    });
    if let Some(principal) = found {
      let slates_db::catalog::Principal::Uid { uid, .. } = principal else {
        panic!("a channel authenticated as something other than a host account: {principal:?}");
      };
      return uid;
    }
    assert!(
      started.elapsed() < TAKEN_WAIT,
      "a shard took client {client_id}"
    );
    std::thread::yield_now();
  }
}
