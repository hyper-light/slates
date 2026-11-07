//! Enrolled consumers on every platform (§4.13 "Principals", AC-2.13 / T-2.15): a consumer enrolled under the host
//! account its workload's channel authenticated as binds that channel, holds only the rights shared with it, and loses
//! them all when revoked. Driven through the product's client against an in-process two-shard daemon, so it runs on
//! every lane CI has; the older flow in `daemon.rs` drives a raw ring client and runs on Unix alone. The account is the
//! one the daemon recorded for the channel: on Unix the peer's uid, on macOS and Windows the daemon's own user, since
//! the bootstrap object's mode is the authentication (on Windows that user is recorded as uid 0; GAPS, A-237).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::panic_in_result_fn
)]

mod common;

use std::time::{Duration, Instant};

use slates_client::ClientError;
use slates_ipc::protocol::{Principal, Refusal, Rights};
use slates_server::daemon::OBSERVE_BUDGET_NS;
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: how long the test waits for a shard to have taken a just-connected client.
const TAKEN_WAIT: Duration = Duration::from_secs(20);

fn daemon(name: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("consumers-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-consumers-{name}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  (daemon, instance)
}

/// The host principal the daemon authenticated `client_id`'s channel as, asked of each shard until one has taken the
/// client.
fn channel_principal(daemon: &Daemon, client_id: u32) -> slates_db::catalog::Principal {
  let started = Instant::now();
  loop {
    let found = daemon.shards().iter().copied().find_map(|shard| {
      daemon
        .observation(
          Some(shard),
          OBSERVE_BUDGET_NS,
          move |state: &mut ShardState| {
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
      return principal;
    }
    assert!(
      started.elapsed() < TAKEN_WAIT,
      "a shard took client {client_id}"
    );
    std::thread::yield_now();
  }
}

/// The account a consumer of `channel`'s host account is enrolled under: the channel's uid.
fn own_account(channel: &slates_db::catalog::Principal) -> u32 {
  match channel {
    slates_db::catalog::Principal::Uid { uid, .. } => *uid,
    other => panic!("a channel authenticated as something other than a host account: {other:?}"),
  }
}

/// AC-2.13 / T-2.15 (§4.13 "Principals", "Access lists"). Do: on a two-shard daemon, enroll a consumer under
/// the host account the workload's channel authenticated as, bind the workload's channel to it, create a volume as the
/// owner, read it as the consumer before and after the owner shares it for reading, destroy it as the consumer, then
/// revoke the consumer and read again. Expect: the channel binds; the unshared volume is `Forbidden`; the shared one
/// reads and refuses a destroy; after the revocation every verb is `ConsumerRevoked`. Measured 2026-11-07 (diag
/// `consumers-1`, main `4326a9d8`): green on ubuntu x86_64 and arm64, Windows x86_64 and arm64, and macOS.
#[test]
fn a_consumer_of_the_channels_own_account_attests_and_holds_only_what_is_shared_on_every_platform()
{
  let (daemon, instance) = daemon("own-account");
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut owner = common::client::connect(&instance);
  let mut workload = common::client::connect(&instance);
  let account = own_account(&channel_principal(&daemon, workload.client_id()));
  let proof = slates_server::landing::enroll_proof(&secret, account);
  let (consumer, capability) = owner.enroll(account, proof).unwrap();
  workload.attest(consumer, &capability).unwrap();
  let volume = owner.create(&common::client::scratch("shared")).unwrap();
  assert!(
    matches!(
      workload.status(volume),
      Err(ClientError::Refused(Refusal::Forbidden { .. }))
    ),
    "an unshared volume is forbidden to the consumer"
  );
  owner
    .share(
      volume,
      Principal::Consumer {
        host: common::this_host(),
        account,
        consumer,
      },
      Rights {
        read: true,
        write: false,
        admin: false,
      },
    )
    .unwrap();
  assert!(workload.status(volume).is_ok(), "the shared volume reads");
  assert!(
    matches!(
      workload.destroy(volume),
      Err(ClientError::Refused(Refusal::Forbidden { .. }))
    ),
    "a read share grants no admin: the destroy is refused"
  );
  let revoke = slates_server::landing::revoke_proof(&secret, consumer);
  owner.revoke(consumer, revoke).unwrap();
  assert!(
    matches!(
      workload.status(volume),
      Err(ClientError::Refused(Refusal::ConsumerRevoked))
    ),
    "a revoked consumer's next verb is refused before any effect"
  );
  drop(workload);
  drop(owner);
  daemon.stop();
}
