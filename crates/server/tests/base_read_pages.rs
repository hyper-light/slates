//! `read_base` at a base file's scale (§4.4 `read_base`, A-267): the entry as the disk holds it now, however large,
//! answered a reply at a time, each page with the file's size and its fingerprint's stamp, and walked whole by the
//! client. Before A-267 the daemon read the whole file into its memory and answered it in one reply, refused "reply too
//! large for the bulk area" past about four kilobytes, so an agent reconciling a drifted entry could read no real file.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result,
  clippy::disallowed_methods
)]
// An overlay over a real host directory: macOS and Linux.
#![cfg(unix)]

use slates_client::{Client, ClientError, CreateSpec, NamePolicy, Refusal, SizeClass, VolumeId};
use slates_ipc::protocol::{ReadAt, ReplyBody, RequestBody};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::connect;
use common::target::{TargetDir, target_dir};

/// Shape: one shard, the overlay's owner.
const TEST_SHARDS: u16 = 1;
/// Shape: a base file of 100 KiB: some twenty-five replies' worth of bytes.
const BIG: usize = 100 << 10;
/// Shape: the overlay's growth bound.
const OVERLAY_BYTES: u64 = 1 << 26;

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-base-read-pages-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-base-read-pages-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// `len` bytes no run of which repeats within a page, so a page read at the wrong offset is told apart.
fn patterned(len: usize, salt: u8) -> Vec<u8> {
  (0..len)
    .map(|n| u8::try_from((n * 31 + n / 251) % 251).unwrap() ^ salt)
    .collect()
}

/// An overlay of `base`.
fn overlay(client: &mut Client, base: &TargetDir) -> VolumeId {
  client
    .create(&CreateSpec {
      name: "overlay".into(),
      size: SizeClass::Dynamic { max: OVERLAY_BYTES },
      names: NamePolicy::Exact,
      require_locked: false,
      base: Some(base.path.clone()),
    })
    .unwrap()
}

/// One page of `/big` from `offset` through the wire: its bytes, the file's size and the stamp.
fn page(client: &mut Client, volume: VolumeId, offset: u64) -> (Vec<u8>, u64, u64) {
  let reply = client.call(&RequestBody::ReadBase {
    volume,
    path: "/big".to_owned(),
    offset,
  });
  let Ok(ReplyBody::BaseBytes {
    bytes,
    total,
    stamp,
  }) = reply
  else {
    panic!("a page of the base file: {reply:?}");
  };
  (bytes, total, stamp)
}

/// A-267 (§4.4 `read_base`). Do: seed a base file of 100 KiB and read it through the client. Expect: every byte, as
/// the disk holds it. Before A-267 the read was refused "reply too large for the bulk area".
#[test]
fn a_base_file_larger_than_one_reply_is_read_whole() {
  let (daemon, instance) = daemon("whole");
  let mut client = connect(&instance);
  let base = target_dir();
  let bytes = patterned(BIG, 0);
  base.seed("big", &bytes);
  let volume = overlay(&mut client, &base);
  assert_eq!(client.read_base(volume, "/big").unwrap(), bytes);
  drop(client);
  daemon.stop();
}

/// A-267 (§4.4 `read_base`). Do: seed a base file of 100 KiB, read its first two pages through the wire, append to it
/// on the disk, then read the page after. Expect: the first two pages share one stamp and name the file's size, each
/// page holds the file's bytes at its offset and is no larger than a reply leaves room for, and the page after the
/// change carries another stamp and the new size: a reader joining pages tells the two states apart, as the client
/// does by refusing the mix (`ChangedWhileRead`).
#[test]
fn a_base_file_changed_between_pages_is_stamped_apart() {
  let (daemon, instance) = daemon("stamp");
  let mut client = connect(&instance);
  let base = target_dir();
  let bytes = patterned(BIG, 0);
  base.seed("big", &bytes);
  let volume = overlay(&mut client, &base);
  let (first, total, stamp) = page(&mut client, volume, 0);
  assert_eq!(total, u64::try_from(BIG).unwrap());
  assert!(
    !first.is_empty() && first.len() < BIG,
    "a page, not the file"
  );
  assert_eq!(first, bytes[..first.len()]);
  let (second, again, same) = page(&mut client, volume, u64::try_from(first.len()).unwrap());
  assert_eq!((again, same), (total, stamp), "one state, one stamp");
  assert_eq!(second, bytes[first.len()..first.len() + second.len()]);
  let mut grown = bytes.clone();
  grown.extend_from_slice(&patterned(BIG, 7));
  base.seed("big", &grown);
  let read = first.len() + second.len();
  let (_, changed_total, changed) = page(&mut client, volume, u64::try_from(read).unwrap());
  assert_eq!(changed_total, u64::try_from(grown.len()).unwrap());
  assert_ne!(changed, stamp, "the changed file stamps its pages apart");
  drop(client);
  daemon.stop();
}

/// A-267, D-15. Do: send the retired whole-file `Read` for a base file of an overlay. Expect: refused `Unsupported`,
/// naming the paged `ReadRange` that replaced it. The verb keeps its published wire place (`tests/wire_order.rs` in
/// slates-ipc), so no tag after it moves, and nothing serves it.
#[test]
fn the_retired_whole_file_read_is_refused_naming_the_paged_read() {
  let (daemon, instance) = daemon("retired");
  let mut client = connect(&instance);
  let base = target_dir();
  base.seed("big", &patterned(BIG, 0));
  let volume = overlay(&mut client, &base);
  let reply = client.call(&RequestBody::Read {
    volume,
    path: "/big".to_owned(),
    at: ReadAt::Head,
  });
  assert!(
    matches!(
      &reply,
      Err(ClientError::Refused(Refusal::Unsupported { feature })) if feature.contains("ReadRange")
    ),
    "{reply:?}"
  );
  drop(client);
  daemon.stop();
}
