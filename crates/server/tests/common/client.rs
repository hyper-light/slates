//! A client of a test's daemon as the product makes one, on every platform: connected with the product's own deadlines,
//! scratch volumes, and the host account the daemon authenticated a client's channel as. The landing fixtures
//! (`landing.rs`, on every platform since the Windows writer, A-185) build on these; the NFS write-back suite, which the
//! Windows server answers too (A-184, A-192), uses them directly.

use slates_client::{Client, CreateSpec, Deadlines, NamePolicy, SizeClass};

/// The product's own deadlines (`Deadlines::derive` over the anchor's liveness budget and the recovery
/// budget).
pub(crate) fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

/// A client of `instance`, whose daemon started before this: the claim waits while the rendezvous's holder lives
/// (A-468), so a refusal is the test's failure, never a wait (A-571's rule; until 2026-11-20 a five-second retry,
/// `START_WAIT`, goal 88's class C).
pub(crate) fn connect(instance: &str) -> Client {
  Client::connect(instance, deadlines()).unwrap_or_else(|e| panic!("{e}"))
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
  // Asked while the daemon lives: each observation is answered by its shard or refused typed past the observe budget,
  // and a daemon gone fails it at once (A-232's rule; until 2026-11-20 twenty seconds on the wall, `TAKEN_WAIT`).
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
    std::thread::yield_now();
  }
}
