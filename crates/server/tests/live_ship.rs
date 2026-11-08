//! Live shipping's policy (§4.10, A-200): a volume is turned live-shipped and back through the `LiveShip` verb, the
//! setting is one recorded operation on the volume's owner partition, and `list` shows it. Test by use (R5): the real
//! daemon, the real client.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use slates_client::{ClientError, VolumeId};
use slates_ipc::protocol::Refusal;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};

/// Shape: shards per test daemon: two, so the verb is routed from the client's shard to the volume's owner shard when
/// they differ.
const TEST_SHARDS: u16 = 2;

fn daemon(name: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{name}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn live_shipped(client: &mut slates_client::Client, volume: VolumeId) -> bool {
  client
    .list()
    .unwrap()
    .into_iter()
    .find(|summary| summary.id == volume)
    .expect("the volume is listed")
    .live_shipped
}

/// A-200: do create a volume (not live-shipped), turn live shipping on, then off. Expect `list` to show false, true,
/// false, each `LiveShip` to answer the setting it recorded, and a volume no one created to be refused `NotFound`.
#[test]
fn a_volume_is_turned_live_shipped_and_back_and_list_shows_it() {
  let (daemon, instance) = daemon("live-ship");
  let mut client = connect(&instance);
  let volume = client.create(&scratch("shipped")).unwrap();
  let before = live_shipped(&mut client, volume);
  let on = client.live_ship(volume, true).unwrap();
  let while_on = live_shipped(&mut client, volume);
  let off = client.live_ship(volume, false).unwrap();
  let after = live_shipped(&mut client, volume);
  let unknown = client.live_ship(VolumeId { bytes: [0; 16] }, true);
  daemon.stop();
  assert!(!before, "a new volume is not live-shipped");
  assert!(on && while_on, "turned on, it lists as live-shipped");
  assert!(!off && !after, "turned off, it lists as not");
  assert!(
    matches!(unknown, Err(ClientError::Refused(Refusal::NotFound))),
    "a volume no one created is not found: {unknown:?}"
  );
}
