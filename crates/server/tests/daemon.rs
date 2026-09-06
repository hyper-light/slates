//! The daemon's tests (Phase 2 task 4; §4.4 "Operations", §4.7, §4.9 "Exactly-once", §4.13,
//! AC-2.8): a daemon started in this process over a fresh segment, a client connected through
//! the real rendezvous, and the lifecycle verbs driven over the rings: create (scratch and
//! overlay, inline and through the bulk area), snapshot, clone, attach with the lease, a
//! second holder refused, detach releasing the lease, resize, status, list, destroy in
//! cooperative slices, a grant refused by kind, a retry returning the retained reply, and
//! acknowledgement releasing it.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use slates_ipc::protocol::{
  Direction, Intent, NamePolicy, Refusal, ReplyBody, RequestBody, SizeClass, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

/// Shape: the probe budget of the quick profile these tests measure (milliseconds); the
/// numbers are inputs to derivations, not gates here.
const PROBE_MS: u64 = 5;
/// Shape: the reply deadline (nanoseconds): five seconds, far past any served verb.
const DEADLINE_NS: u64 = 5_000_000_000;
/// Shape: how long a client waits for a full ring to drain before giving up.
const CREDIT_WAIT: Duration = Duration::from_secs(5);
/// Shape: shards per test daemon: two, so a client lands on a shard other than the control
/// shard's and the cross-shard hand-off runs.
const TEST_SHARDS: u16 = 2;

fn profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
}

/// A client: its ring end and its request sequence.
struct Client {
  end: ClientEnd,
  client: u32,
  sequence: u32,
}

impl Client {
  fn connect(instance: &str) -> Client {
    let started = Instant::now();
    loop {
      match connect(instance) {
        Ok(connected) => {
          let client = connected.region.client_id();
          return Client {
            end: ClientEnd::with_doorbell(connected.region, connected.doorbell),
            client,
            sequence: 0,
          };
        }
        Err(IpcError::DaemonUnavailable { .. }) if started.elapsed() < CREDIT_WAIT => {
          std::hint::spin_loop();
        }
        Err(e) => panic!("{e}"),
      }
    }
  }

  /// Sends `body` as the next request and waits for the reply.
  fn call(&mut self, body: &RequestBody) -> ReplyBody {
    self.sequence += 1;
    let id = RequestId {
      client: self.client,
      sequence: self.sequence,
    };
    self.call_as(id, body)
  }

  /// Sends `body` under `id` (a retry reuses an id) and waits for the reply.
  fn call_as(&mut self, id: RequestId, body: &RequestBody) -> ReplyBody {
    let index = self.end.next_request_index();
    let slot = pack(
      self.end.region_mut(),
      Direction::Request,
      index,
      id.word(),
      body,
    )
    .unwrap();
    let started = Instant::now();
    loop {
      match self.end.send(&slot) {
        Ok(()) => break,
        Err(IpcError::RingFull) if started.elapsed() < CREDIT_WAIT => std::hint::spin_loop(),
        Err(e) => panic!("{e}"),
      }
    }
    let reply = self
      .end
      .wait(Some(DEADLINE_NS))
      .unwrap_or_else(|e| panic!("{e} waiting for {id:?} {body:?}"));
    assert_eq!(reply.request, id.word(), "the reply answers the request");
    unpack(self.end.region(), reply.kind, &reply.payload).unwrap()
  }
}

fn daemon(name: &str) -> (Daemon, String) {
  let profile = profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{name}"),
    },
  )
  .unwrap();
  (daemon, instance)
}

fn scratch(name: &str) -> RequestBody {
  RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// Create, a duplicate name, snapshot, clone: the ids and the refusal.
fn create_snapshot_clone(
  client: &mut Client,
) -> (
  slates_ipc::protocol::VolumeId,
  slates_ipc::protocol::VolumeId,
) {
  let ReplyBody::Created { id } = client.call(&scratch("one")) else {
    panic!("create");
  };
  assert!(matches!(
    client.call(&scratch("one")),
    ReplyBody::Refused { refusal: Refusal::AlreadyExists { existing } } if existing == id
  ));
  let ReplyBody::Snapshotted { id: snap } = client.call(&RequestBody::Snapshot { volume: id })
  else {
    panic!("snapshot");
  };
  let ReplyBody::Cloned { id: clone } = client.call(&RequestBody::Clone {
    volume: id,
    snapshot: snap,
    name: "one-clone".into(),
  }) else {
    panic!("clone");
  };
  assert_ne!(clone, id);
  (id, clone)
}

/// Attach for writing takes the lease; status shows it.
fn attach_and_status(client: &mut Client, id: slates_ipc::protocol::VolumeId) -> u64 {
  let ReplyBody::Attached {
    attachment,
    lease_epoch,
    path,
  } = client.call(&RequestBody::Attach {
    volume: id,
    snapshot: None,
    intent: Intent::Write,
  })
  else {
    panic!("attach");
  };
  assert_eq!(lease_epoch, Some(1));
  assert_eq!(path, None, "no bridge yet");
  let ReplyBody::Status { report } = client.call(&RequestBody::Status { volume: id }) else {
    panic!("status");
  };
  assert_eq!(report.name, "one");
  assert_eq!(report.lease_epoch, Some(1));
  assert_eq!(report.attachments, 1);
  assert_eq!(report.snapshots, 1);
  assert_eq!(report.watcher, "scratch");
  attachment
}

/// List shows both volumes; detach releases the lease.
fn list_and_detach(client: &mut Client, id: slates_ipc::protocol::VolumeId, attachment: u64) {
  let ReplyBody::Listed { volumes } = client.call(&RequestBody::List) else {
    panic!("list");
  };
  let mut names: Vec<String> = volumes.iter().map(|v| v.name.clone()).collect();
  names.sort();
  assert_eq!(names, vec!["one", "one-clone"]);
  assert!(matches!(
    client.call(&RequestBody::Detach { attachment }),
    ReplyBody::Detached
  ));
  let ReplyBody::Status { report } = client.call(&RequestBody::Status { volume: id }) else {
    panic!("status");
  };
  assert_eq!(
    report.lease_epoch, None,
    "the last write attachment released the lease"
  );
}

/// Resize, destroy in slices; the clone survives its origin's destroy.
fn resize_and_destroy(client: &mut Client, id: slates_ipc::protocol::VolumeId) {
  assert!(matches!(
    client.call(&RequestBody::Resize {
      volume: id,
      size: SizeClass::Bounded { limit: 2 << 20 }
    }),
    ReplyBody::Resized
  ));
  assert!(matches!(
    client.call(&RequestBody::Destroy { volume: id }),
    ReplyBody::Destroyed
  ));
  assert!(matches!(
    client.call(&RequestBody::Status { volume: id }),
    ReplyBody::Refused {
      refusal: Refusal::Destroying | Refusal::NotFound
    }
  ));
  let started = Instant::now();
  loop {
    let ReplyBody::Listed { volumes } = client.call(&RequestBody::List) else {
      panic!("list");
    };
    let names: Vec<String> = volumes.iter().map(|v| v.name.clone()).collect();
    if names == vec!["one-clone"] {
      break;
    }
    assert!(
      started.elapsed() < CREDIT_WAIT,
      "destroy completes in slices: {names:?}"
    );
  }
}

/// The worked example of §2.2: create, snapshot, clone, attach, status, list, detach,
/// resize, destroy, over one client through the real rendezvous.
fn lifecycle_scenario() {
  let (daemon, instance) = daemon("lifecycle");
  let mut client = Client::connect(&instance);
  let (id, _clone) = create_snapshot_clone(&mut client);
  let attachment = attach_and_status(&mut client, id);
  list_and_detach(&mut client, id, attachment);
  resize_and_destroy(&mut client, id);
  daemon.stop();
}

/// Exactly-once (§4.9): a retry under the same request id returns the retained reply without
/// executing again; an acknowledgement releases it and a later retry is a stale duplicate.
fn rifl_scenario() {
  let (daemon, instance) = daemon("rifl");
  let mut client = Client::connect(&instance);
  client.sequence += 1;
  let id = RequestId {
    client: client.client,
    sequence: client.sequence,
  };
  let ReplyBody::Created { id: volume } = client.call_as(id, &scratch("once")) else {
    panic!("create");
  };
  // The retry: the same reply, no second volume.
  let ReplyBody::Created { id: again } = client.call_as(id, &scratch("once")) else {
    panic!("retry");
  };
  assert_eq!(again, volume);
  let ReplyBody::Listed { volumes } = client.call(&RequestBody::List) else {
    panic!("list");
  };
  assert_eq!(volumes.len(), 1, "the retry did not execute again");
  assert!(matches!(
    client.call(&RequestBody::Acknowledge { up_to: id.sequence }),
    ReplyBody::Acknowledged
  ));
  assert!(matches!(
    client.call_as(id, &scratch("once")),
    ReplyBody::Refused {
      refusal: Refusal::DuplicateRequest
    }
  ));
  daemon.stop();
}

/// Leases (D-16, AC-2.4): a second principal cannot attach for writing while the lease is
/// held; the same client renews; a read attachment needs no lease.
fn lease_scenario() {
  let (daemon, instance) = daemon("leases");
  let mut a = Client::connect(&instance);
  let mut b = Client::connect(&instance);
  let ReplyBody::Created { id } = a.call(&scratch("shared")) else {
    panic!("create");
  };
  let ReplyBody::Attached { lease_epoch, .. } = a.call(&RequestBody::Attach {
    volume: id,
    snapshot: None,
    intent: Intent::Write,
  }) else {
    panic!("attach");
  };
  assert_eq!(lease_epoch, Some(1));
  // Both clients are the same uid on one host, so the same principal: the "second holder"
  // is the same principal renewing (the multi-principal case is the fleet's, Phase 8); a
  // read attachment takes no lease either way.
  let ReplyBody::Attached { lease_epoch, .. } = b.call(&RequestBody::Attach {
    volume: id,
    snapshot: None,
    intent: Intent::Read,
  }) else {
    panic!("attach read");
  };
  assert_eq!(lease_epoch, None);
  let ReplyBody::Attached { lease_epoch, .. } = b.call(&RequestBody::Attach {
    volume: id,
    snapshot: None,
    intent: Intent::Write,
  }) else {
    panic!("attach write");
  };
  assert_eq!(
    lease_epoch,
    Some(1),
    "the same principal renews with its epoch"
  );
  daemon.stop();
}

/// AC-2.8: the grant kind is refused on the ring with a typed refusal, and counted.
fn grant_scenario() {
  let (daemon, instance) = daemon("grant");
  let mut client = Client::connect(&instance);
  assert!(matches!(
    client.call(&RequestBody::Grant { request: 1 }),
    ReplyBody::Refused { refusal: Refusal::GrantChannelRefused { channel } } if channel == "ring"
  ));
  daemon.stop();
}

/// Landings and grants through the server (§4.15, task 8): a grant cannot be created on the
/// ring (its kind is refused); a landing into a target that cannot be opened is refused with
/// no write; the caller's grants and the audit log read empty before any landing. The full
/// plan-present-grant-execute flow needs a real target and the control channel, so it runs in
/// the Linux CI lane (`crates/server/tests/landing.rs`), not here.
fn landing_refusal_scenario() {
  let (daemon, instance) = daemon("landing");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("work")) else {
    panic!("create");
  };
  // A target that does not exist is refused before any write (no filesystem entry created).
  assert!(matches!(
    client.call(&RequestBody::Land {
      volume: id,
      snapshot: None,
      target: "/nonexistent/slates/land/target".to_owned(),
      filter: slates_ipc::protocol::Filter::default(),
      grant: None,
    }),
    ReplyBody::Refused {
      refusal: Refusal::TargetUnavailable { .. }
    }
  ));
  // The caller has no grants and the audit log is empty (the refused landing recorded nothing).
  assert!(matches!(
    client.call(&RequestBody::Grants),
    ReplyBody::Grants { grants } if grants.is_empty()
  ));
  assert!(matches!(
    client.call(&RequestBody::Audit { since: 0 }),
    ReplyBody::Audit { records } if records.is_empty()
  ));
  daemon.stop();
}

/// A request larger than a slot's payload travels through the bulk area: a long name and
/// an overlay over this workspace's own source tree (read only).
fn bulk_and_overlay_scenario() {
  let (daemon, instance) = daemon("bulk");
  let mut client = Client::connect(&instance);
  let long = "x".repeat(200);
  let ReplyBody::Created { id } = client.call(&scratch(&long)) else {
    panic!("create long");
  };
  let ReplyBody::Status { report } = client.call(&RequestBody::Status { volume: id }) else {
    panic!("status");
  };
  assert_eq!(
    report.name, long,
    "the reply travelled through the bulk area too"
  );
  let base = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
  let ReplyBody::Created { id: overlay } = client.call(&RequestBody::Create {
    name: "over".into(),
    size: SizeClass::Dynamic { max: 1 << 24 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: Some(base.to_owned()),
  }) else {
    panic!("create overlay");
  };
  let ReplyBody::BaseBytes { bytes } = client.call(&RequestBody::ReadBase {
    volume: overlay,
    path: "/lib.rs".into(),
  }) else {
    panic!("read_base");
  };
  assert!(
    bytes.starts_with(b"//! `slates-server`"),
    "the base is read from the disk"
  );
  let ReplyBody::Status { report } = client.call(&RequestBody::Status { volume: overlay }) else {
    panic!("status");
  };
  assert!(report.drifted.is_empty());
  assert!(matches!(
    client.call(&RequestBody::Pin {
      volume: overlay,
      paths: Some(vec!["/lib.rs".into()])
    }),
    ReplyBody::Pinned { entries: 1 }
  ));
  assert!(matches!(
    client.call(&RequestBody::ReadBase {
      volume: id,
      path: "/x".into()
    }),
    ReplyBody::Refused {
      refusal: Refusal::Unsupported { .. }
    }
  ));
  assert!(matches!(
    client.call(&RequestBody::Create {
      name: "bad".into(),
      size: SizeClass::Bounded { limit: 1 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: Some("/nonexistent/slates/base".into())
    }),
    ReplyBody::Refused {
      refusal: Refusal::BaseUnavailable { .. }
    }
  ));
  daemon.stop();
}

/// The scenarios run one daemon at a time (each daemon runs shard threads that spin while a
/// client is active; several at once would starve each other on one machine).
#[test]
fn the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals() {
  lifecycle_scenario();
  rifl_scenario();
  lease_scenario();
  grant_scenario();
  landing_refusal_scenario();
  bulk_and_overlay_scenario();
}

/// Shape: a version slab large enough to physically hold several volumes' inode and trie nodes, yet
/// small enough that one big-quota volume's inode allowance reserves all of it above the copy-up
/// headroom. So a further volume is refused by the §4.2 *reservation* while physical slots plainly
/// remain — the honesty the reservation adds over the bare per-volume cap (a raw `SlabFull` would
/// only fire once the slab were physically exhausted, which it is not here).
const SMALL_VERSION_SLAB: usize = 64;
/// Shape: refused-create attempts against a full version budget — enough that, were a partial volume
/// allocated and leaked on each refusal (the pre-reorder bug), the trie slab would physically fill
/// (a volume needs several trie nodes; 64 slots hold only a handful) and later refusals would turn
/// into `SlabFull`. With the reservation checked before any allocation, all stay `BudgetExceeded`.
const LEAK_PROBES: usize = 24;

/// A test daemon whose inode-version slab is `max_inodes` slots, on a single shard so every volume
/// shares the one version budget (with two shards, volumes route to separate budgets and never
/// contend). Otherwise the derived config the other scenarios use.
fn capped_daemon(name: &str, max_inodes: usize) -> (Daemon, String) {
  let profile = profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance).with_shards(1);
  config.store.max_inodes = max_inodes;
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{name}"),
    },
  )
  .unwrap();
  (daemon, instance)
}

/// The §4.2 inode-dimension *reservation*, through the real create and destroy verbs (no mount): a
/// volume's inode allowance is reserved against the shard's version slab at create, so a second
/// volume whose allowance the slab cannot also back is refused (`BudgetExceeded`) even though byte
/// space plainly remains — the disjoint reservation the bare per-volume cap does not give — and the
/// slab returns on destroy so a later volume is admitted again. Non-vacuous: the byte budget is far
/// larger than these small quotas, so only the version reservation can refuse the second volume;
/// without it, both would be created.
fn version_reservation_scenario() {
  let (daemon, instance) = capped_daemon("versions", SMALL_VERSION_SLAB);
  let mut client = Client::connect(&instance);
  let sized = |name: &str| RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  };
  // The first volume's inode allowance fills the version slab (less the one-slot copy-up headroom).
  let ReplyBody::Created { id: first } = client.call(&sized("first")) else {
    panic!("first create");
  };
  // Many further creates cannot be backed by the slab, so admission refuses each — not for want of
  // bytes. Every one must refuse at the *reservation* (`BudgetExceeded`), never with `SlabFull`:
  // that is the leak proof. The reservation is taken before `Volume::create`, so a refusal allocates
  // no root inode, trie or dir. Were a partial volume leaked on each refusal instead, these probes
  // would physically exhaust the trie slab within a handful of attempts and later refusals would
  // become `SlabFull` (a `BadRequest`) — which this asserts never happens.
  for probe in 0..LEAK_PROBES {
    let name = format!("probe-{probe}");
    match client.call(&sized(&name)) {
      ReplyBody::Refused {
        refusal: Refusal::BudgetExceeded { .. },
      } => {}
      other => {
        panic!("probe {probe} must refuse at the reservation, not leak into SlabFull: {other:?}")
      }
    }
  }
  // Destroy the first; its reserved slots return to the slab as the destroy completes in slices.
  assert!(matches!(
    client.call(&RequestBody::Destroy { volume: first }),
    ReplyBody::Destroyed
  ));
  let started = Instant::now();
  loop {
    let ReplyBody::Listed { volumes } = client.call(&RequestBody::List) else {
      panic!("list");
    };
    if volumes.is_empty() {
      break;
    }
    assert!(
      started.elapsed() < CREDIT_WAIT,
      "destroy completes in slices: {volumes:?}"
    );
  }
  // With the slab returned, a fresh volume is admitted again — the reservation is released on teardown.
  assert!(
    matches!(client.call(&sized("third")), ReplyBody::Created { .. }),
    "the slab freed by destroy backs a new volume's allowance"
  );
  daemon.stop();
}

/// AC-2: §4.2 inode-dimension reservation — the disjoint version-slab reservation and its release on
/// teardown, driven through the real create and destroy verbs.
#[test]
fn the_inode_allowance_is_reserved_against_the_version_slab_and_released_on_teardown() {
  version_reservation_scenario();
}

/// The inode reservation moves with the policy on resize (§4.2): a volume whose allowance fills the
/// version slab, resized down, returns slots so another volume is admitted — proof the reservation
/// tracks the resized allowance, not the create-time one. Non-vacuous: without the re-derivation the
/// resized volume keeps its old (larger) reservation and the second volume stays refused.
fn version_reservation_moves_on_resize_scenario() {
  let (daemon, instance) = capped_daemon("resize-versions", SMALL_VERSION_SLAB);
  let mut client = Client::connect(&instance);
  let sized = |name: &str, limit: u64| RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  };
  // A big-quota volume's allowance fills the version slab.
  let ReplyBody::Created { id: filler } = client.call(&sized("filler", 1 << 20)) else {
    panic!("filler create");
  };
  // A second volume, however small, cannot be backed while the filler holds the whole slab.
  assert!(
    matches!(
      client.call(&sized("tenant", 1)),
      ReplyBody::Refused {
        refusal: Refusal::BudgetExceeded { .. }
      }
    ),
    "the version slab is full while the filler holds its whole allowance"
  );
  // Resize the filler down to a tiny quota: its inode allowance shrinks and returns slots.
  assert!(
    matches!(
      client.call(&RequestBody::Resize {
        volume: filler,
        size: SizeClass::Bounded { limit: 1 }
      }),
      ReplyBody::Resized
    ),
    "resize-down accepted"
  );
  // The returned slots back the small volume now — proof the reservation moved with the resize.
  assert!(
    matches!(client.call(&sized("tenant", 1)), ReplyBody::Created { .. }),
    "the slots freed by the resize-down back a new volume"
  );
  daemon.stop();
}

/// AC-2: §4.2 inode reservation tracks a resized allowance — a resize-down returns version slots.
#[test]
fn the_inode_reservation_moves_with_the_allowance_on_resize() {
  version_reservation_moves_on_resize_scenario();
}
