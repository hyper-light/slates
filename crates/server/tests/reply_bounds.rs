//! Every list a verb's own reply carries fits one reply (§4.3: a reply is one bulk chunk; A-267): an attachment's
//! advance, a rewitness, a presentation's conflicts and a granted landing's conflicts, each at the scale a real change
//! reaches. The reply carries the first items that fit and counts the rest. Before A-267 each was one list in one reply,
//! and past what a reply chunk holds the daemon answered `BadRequest { reason: "reply too large for the bulk area" }`
//! in its place: after an advance or a rewitness had already taken effect, after a presentation was recorded, and in
//! place of a landing's typed refusal.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result,
  clippy::disallowed_methods
)]
// An overlay over a real host directory, and a landing onto one: macOS and Linux.
#![cfg(unix)]

use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, CreateSpec, NamePolicy, Refusal, SizeClass, VolumeId};
use slates_ipc::protocol::{Filter, Intent, ReplyBody, RequestBody};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::connect;
use common::target::{TargetDir, target_dir};

/// Shape: one shard, the owner of every volume a test makes.
const TEST_SHARDS: u16 = 1;
/// Shape: the paths each test changes: several replies' worth at these names.
const MANY: usize = 400;
/// Shape: an overlay's growth bound: room for every file a test writes through it.
const OVERLAY_BYTES: u64 = 1 << 26;
/// Shape: the permission bits of a file a test writes.
const MODE: u32 = 0o644;
/// Shape: how long a base's watcher may take to see every change a test makes on the disk.
const WATCH_WAIT: Duration = Duration::from_secs(30);

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-reply-bounds-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-reply-bounds-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// The `n`th path a test changes: long enough that a few dozen fill a reply.
fn file(n: usize) -> String {
  format!("a-file-named-long-enough-to-fill-a-reply-sooner-{n:05}.txt")
}

/// An overlay of `base`, where every one of [`MANY`] files is seeded with `bytes`.
fn overlay(client: &mut Client, base: &TargetDir, bytes: &[u8]) -> VolumeId {
  for n in 0..MANY {
    base.seed(&file(n), bytes);
  }
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

/// Writes `bytes` over every one of [`MANY`] files of `volume`, under one write attachment.
fn write_every_file(client: &mut Client, volume: VolumeId, bytes: &[u8]) {
  let attached = client.attach(volume, None, Intent::Write).unwrap();
  for n in 0..MANY {
    client
      .fs_write(
        (volume, attached.attachment),
        &format!("/{}", file(n)),
        bytes,
        MODE,
      )
      .unwrap();
  }
  client.detach(attached.attachment).unwrap();
}

/// A-267 (§4.16 "Attachments and versions"). Do: attach a reader to an empty green, merge a work that writes 400
/// files, and advance the attachment to the head. Expect: `Advanced` at the head, carrying the first changed paths in
/// order (each a file the span changed) and counting the rest, the two making 400, the count above zero (the test
/// outgrows one reply). Before A-267 the advance re-pinned the attachment and its reply was refused "reply too large".
#[test]
fn an_advance_over_more_paths_than_one_reply_holds_is_answered() {
  let (daemon, instance) = daemon("advance");
  let mut client = connect(&instance);
  let green = client.create_green("green", false).unwrap();
  let attached = client.attach(green, None, Intent::Read).unwrap();
  let (seed, _) = client.create_work(green, "seed").unwrap();
  for n in 0..MANY {
    client.edit(seed, &file(n), 0, 0, b"bytes").unwrap();
  }
  let Ok(slates_client::Submitted::Accepted(head)) = client.submit(seed) else {
    panic!("a lone work merges");
  };
  let advanced = client.call(&RequestBody::Advance {
    attachment: attached.attachment,
    version: None,
  });
  let Ok(ReplyBody::Advanced {
    version,
    invalidated,
    invalidated_elided,
  }) = advanced
  else {
    panic!("the advance is answered: {advanced:?}");
  };
  assert_eq!(version, head);
  let changed: Vec<String> = (0..MANY).map(file).collect();
  assert!(
    invalidated_elided > 0,
    "the test outgrows one reply: all {} carried",
    invalidated.len()
  );
  assert_eq!(invalidated.len() + invalidated_elided as usize, MANY);
  assert_eq!(
    invalidated,
    changed[..invalidated.len()],
    "the reply carries the first changed paths, in order"
  );
  drop(client);
  daemon.stop();
}

/// A-267 (§4.15 "Drift"). Do: overlay a base of 400 files, pin them, change every one on the disk, wait for the drift,
/// and rewitness every drifted entry. Expect: `Rewitnessed`, carrying some of the 400 paths and counting the rest (the
/// count above zero), and the volume's drift empty after it: the rewitness took every one. Before A-267 the rewitness
/// took effect and its reply was refused "reply too large".
#[test]
fn a_rewitness_of_more_drift_than_one_reply_holds_is_answered() {
  let (daemon, instance) = daemon("rewitness");
  let mut client = connect(&instance);
  let base = target_dir();
  let volume = overlay(&mut client, &base, b"before");
  assert_eq!(client.pin(volume, None).unwrap(), MANY as u64);
  for n in 0..MANY {
    base.seed(&file(n), b"after, and longer");
  }
  let started = Instant::now();
  loop {
    let report = client.status(volume).unwrap();
    if report.drifted.len() + report.drifted_elided as usize >= MANY {
      break;
    }
    assert!(started.elapsed() < WATCH_WAIT, "the drift is seen");
    std::thread::sleep(Duration::from_millis(100));
  }
  let rewitnessed = client.call(&RequestBody::Rewitness {
    volume,
    paths: None,
  });
  let Ok(ReplyBody::Rewitnessed {
    paths,
    paths_elided,
  }) = rewitnessed
  else {
    panic!("the rewitness is answered: {rewitnessed:?}");
  };
  assert!(paths_elided > 0, "the test outgrows one reply");
  assert_eq!(paths.len() + paths_elided as usize, MANY);
  let report = client.status(volume).unwrap();
  assert_eq!(
    (report.drifted.len(), report.drifted_elided),
    (0, 0),
    "every entry was rewitnessed"
  );
  drop(client);
  daemon.stop();
}

/// A-267 (§4.15 step 2). Do: overlay a base of 400 files, write every one through the volume, change every one on the
/// disk, and present the landing of the head into the base. Expect: `GrantRequired`, carrying conflicts and counting
/// the rest (the count above zero), the two making 400; and the presentation's entries, read a page at a time, all 400
/// in conflict. Before A-267 the presentation was recorded and its reply refused "reply too large", so the landing it
/// presented could not be named.
#[test]
fn a_presentation_with_more_conflicts_than_one_reply_holds_is_answered() {
  let (daemon, instance) = daemon("presented");
  let mut client = connect(&instance);
  let base = target_dir();
  let volume = overlay(&mut client, &base, b"before");
  write_every_file(&mut client, volume, b"the volume's");
  for n in 0..MANY {
    base.seed(&file(n), b"the disk's, and longer");
  }
  let presented = client.call(&RequestBody::Land {
    volume,
    snapshot: None,
    target: base.path.clone(),
    filter: Filter::default(),
    grant: None,
    signature: None,
  });
  let Ok(ReplyBody::GrantRequired {
    landing,
    manifest,
    conflicts,
    conflicts_elided,
    ..
  }) = presented
  else {
    panic!("the presentation is answered: {presented:?}");
  };
  assert!(conflicts_elided > 0, "the test outgrows one reply");
  assert_eq!(conflicts.len() + conflicts_elided as usize, MANY);
  let entries = client.landing_entries(landing, manifest).unwrap();
  assert_eq!(
    entries
      .iter()
      .filter(|entry| entry.conflict.is_some())
      .count(),
    MANY,
    "every conflict is read from the presentation's entries"
  );
  drop(client);
  daemon.stop();
}

/// A-267 (§4.15 step 4). Do: overlay a base of 400 files, write every one through the volume, present the landing of
/// the head into the base and approve it, then change every file on the disk and land under the grant. Expect: the
/// landing refused `LandingConflict`, carrying conflicts and counting the rest (above zero), the two making 400, and
/// nothing written. Before A-267 the refusal was replaced by "reply too large".
#[test]
fn a_granted_landing_on_more_conflicts_than_one_reply_holds_is_refused_typed() {
  let (daemon, instance) = daemon("granted");
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let base = target_dir();
  let volume = overlay(&mut client, &base, b"before");
  write_every_file(&mut client, volume, b"the volume's");
  let grant = common::landing::approve(&mut client, &secret, volume, None, &base.path);
  for n in 0..MANY {
    base.seed(&file(n), b"the disk's, and longer");
  }
  let landed = client.land(volume, None, &base.path, Filter::default(), Some(grant));
  let Err(ClientError::Refused(Refusal::LandingConflict {
    entries,
    entries_elided,
  })) = landed
  else {
    panic!("the landing is refused for its conflicts: {landed:?}");
  };
  assert!(entries_elided > 0, "the test outgrows one reply");
  assert_eq!(entries.len() + entries_elided as usize, MANY);
  assert_eq!(
    std::fs::read(format!("{}/{}", base.path, file(0))).unwrap(),
    b"the disk's, and longer",
    "nothing was written"
  );
  drop(client);
  daemon.stop();
}

/// Shape: the bytes of each mount path the mounts test binds: long enough that a few outgrow the status's mount budget.
const MOUNT_PATH_BYTES: usize = 160;

/// A scratch volume of `name`.
fn scratch(client: &mut Client, name: &str) -> Result<VolumeId, ClientError> {
  client.create(&CreateSpec {
    name: name.into(),
    size: SizeClass::Bounded {
      limit: OVERLAY_BYTES,
    },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  })
}

/// Binds host mounts of `volume` at paths of [`MOUNT_PATH_BYTES`] until the partition's attachment table is full (its
/// own bound: refused `TableAtCapacity`): the mounts bound.
fn bind_mounts_to_capacity(client: &mut Client, volume: VolumeId) -> usize {
  for n in 0.. {
    let attached = match client.attach_mount(volume, Intent::Read) {
      Ok(attached) => attached,
      Err(ClientError::Refused(Refusal::TableAtCapacity { table })) if table == "attachments" => {
        return n;
      }
      Err(other) => panic!("mount {n}: {other:?}"),
    };
    let path = format!("/{n:04}-{}", "m".repeat(MOUNT_PATH_BYTES - 6));
    let bound = client.call(&RequestBody::BindMount {
      attachment: attached.attachment,
      path,
    });
    assert!(matches!(bound, Ok(ReplyBody::MountBound)), "{bound:?}");
  }
  panic!("the attachment table is bounded")
}

/// A-267 (§4.4 `status`, GAP-A9-4). Do: bind host mounts of one volume at paths of 160 bytes until the attachment
/// table refuses one more, then ask the volume's status. Expect: the status answered, carrying some mounts and counting
/// the rest, the two making every mount bound. The mount budget (a quarter of a reply) counts each mount's whole
/// encoding; it counted paths alone before, which at the table's bound overshot the budget by twelve bytes a mount and
/// was never refused, since the drift is carried in what room the rest leaves.
#[test]
fn a_status_with_more_mounts_than_its_budget_holds_is_answered() {
  let (daemon, instance) = daemon("mounts");
  let mut client = connect(&instance);
  let volume = scratch(&mut client, "mounted").unwrap();
  let bound = bind_mounts_to_capacity(&mut client, volume);
  eprintln!("mounts bound to the attachment table's bound: {bound}");
  let report = client.status(volume);
  let Ok(report) = report else {
    panic!("the status is answered: {report:?}");
  };
  assert!(
    report.mounts_elided > 0,
    "the test outgrows the mounts' budget: {bound} bound"
  );
  assert_eq!(report.mounts.len() + report.mounts_elided as usize, bound);
  drop(client);
  daemon.stop();
}

/// Shape: a name length no create accepts: a reply chunk's whole size, past what a create's request carries.
const NAME_PAST_EVERY_BOUND: usize = 4096;

/// The longest name a create accepts, found by asking: a name one byte longer is refused `InvalidName`, or is more than
/// the create's request carries (`PayloadTooLarge`). Every create that succeeds on the way makes a scratch volume of its
/// own name.
fn longest_accepted_name(client: &mut Client) -> String {
  let (mut accepted, mut refused) = (1usize, NAME_PAST_EVERY_BOUND);
  while refused - accepted > 1 {
    let middle = accepted + (refused - accepted) / 2;
    match scratch(client, &"n".repeat(middle)) {
      Ok(_) => accepted = middle,
      Err(
        ClientError::Refused(Refusal::InvalidName)
        | ClientError::Ipc(slates_ipc::error::IpcError::PayloadTooLarge { .. }),
      ) => refused = middle,
      Err(other) => panic!("a create of a {middle}-byte name: {other:?}"),
    }
  }
  "n".repeat(accepted)
}

/// A-263, A-267 (§4.4 `create`, `list`, `status`). Do: find the longest name a create accepts (one byte longer is
/// refused `InvalidName`), then list the volume made under it and ask its status, bare and then with host mounts bound
/// to the attachment table's bound. Expect: listed and answered each time, the name whole. A create bounds a name so
/// every reply that names the volume holds it; a status names it beside its other fields.
#[test]
fn a_volume_under_the_longest_name_a_create_accepts_is_listed_and_statused() {
  let (daemon, instance) = daemon("longest-name");
  let mut client = connect(&instance);
  let name = longest_accepted_name(&mut client);
  eprintln!("the longest name a create accepts: {} bytes", name.len());
  let volume = client
    .list()
    .unwrap()
    .into_iter()
    .find(|summary| summary.name == name)
    .map(|summary| summary.id)
    .expect("the volume under the longest name is listed");
  let bare = client.status(volume);
  assert!(
    bare.as_ref().is_ok_and(|report| report.name == name),
    "a {}-byte name's status: {bare:?}",
    name.len()
  );
  let bound = bind_mounts_to_capacity(&mut client, volume);
  let mounted = client.status(volume);
  assert!(
    mounted
      .as_ref()
      .is_ok_and(|report| report.name == name
        && report.mounts.len() + report.mounts_elided as usize == bound),
    "a {}-byte name's status beside {bound} mounts: {mounted:?}",
    name.len()
  );
  drop(client);
  daemon.stop();
}
