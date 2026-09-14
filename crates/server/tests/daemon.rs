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
  AbsenceIs, AttachRequest, CauseRecord, Direction, Filter, GrantScope, Intent, NamePolicy,
  Refusal, ReplyBody, RequestBody, SizeClass, SpanRecord, TelemetryReport, VolumeId, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::host_id_of;
use slates_server::deploy::member_id;
use slates_server::{Daemon, DaemonConfig, DurabilityBound, FleetMembership, SegmentSource};
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

/// A daemon configured as a member of an `f = 1` fleet of three (itself and two peers it never reaches — no
/// transport runs, so no probe, session or record plane is involved), under the operator's `durability`
/// policy. The placement configuration is formed at boot from the declared members (§4.8, boot step 6), so
/// what the policy allows is decided without a network; this is the smallest daemon that has a durability to
/// fall short of.
fn fleet_daemon(name: &str, durability: Option<DurabilityBound>) -> (Daemon, String) {
  let profile = profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  let origin_anchor = slates_db::HostId(host_id_of(&profile.facts.identity));
  let host = member_id(origin_anchor, 0);
  // The peers' ids only need to be distinct from this node's; the configuration is formed over all three.
  let peers = vec![
    slates_db::HostId(host.0.wrapping_add(1)),
    slates_db::HostId(host.0.wrapping_add(2)),
  ];
  let config = DaemonConfig::derive(&profile, &instance)
    .with_shards(TEST_SHARDS)
    .with_fleet(FleetMembership {
      quorum: slates_db::register::Quorum { f: 1 },
      peers,
      host,
      origin_anchor,
      domains: std::collections::BTreeMap::new(),
      regions: std::collections::BTreeMap::new(),
      durability,
      region_mirrors: std::collections::BTreeMap::new(),
    });
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

/// AC (§4.8 "Placement" — "the operator's accepted ε and the coincident-failure size are the durability policy
/// that gates a refusal"; D-14, D-18): a write that would commit a new head or seal is refused, **typed and
/// with the measured shortfall**, while the fleet's configuration cannot hold it to the declared durability;
/// within the policy it is accepted; and with no policy declared it is accepted as before. Three daemons of
/// the same fleet shape (`f = 1`, three members) differ only in their policy: one that accepts no loss under
/// two coincident failures — which an `f = 1` copyset cannot survive, so the configuration's loss is above ε
/// — refuses `Create` with `DurabilityUnmet` carrying that loss, the ε and the failure count, counts it, and
/// still serves reads (`List`, `DaemonStatus`); one that accepts no loss under a **single** failure (no
/// `f = 1` copyset falls wholly inside one host) creates and snapshots; one with no policy does the same.
/// Non-vacuous: the refusing daemon and an accepting one are the same code and fleet shape — only the ε and
/// the failure count decide.
#[test]
fn a_write_the_declared_durability_cannot_cover_is_refused_typed() {
  // The configuration cannot hold a write to this: any f = 1 copyset is lost when its two holders fail at
  // once, so its coincident-loss probability under two failures is positive, above an ε of zero.
  let (strict, strict_instance) = fleet_daemon(
    "durability-strict",
    Some(DurabilityBound {
      accepted_loss: 0.0,
      coincident_failures: 2,
    }),
  );
  let mut client = Client::connect(&strict_instance);
  let reply = client.call(&scratch("short"));
  let ReplyBody::Refused {
    refusal:
      Refusal::DurabilityUnmet {
        coincident_loss,
        accepted_loss,
        coincident_failures,
      },
  } = reply
  else {
    strict.stop();
    panic!("a write the configuration cannot hold to the policy is refused typed, got {reply:?}");
  };
  assert!(
    coincident_loss > accepted_loss,
    "the refusal carries the measured loss ({coincident_loss}) above the accepted ε ({accepted_loss})"
  );
  assert_eq!(
    accepted_loss, 0.0,
    "the ε is the operator's declared accepted loss"
  );
  assert_eq!(
    coincident_failures, 2,
    "the failure count is the one the policy was stated under"
  );
  // Reads continue: the listing answers (empty — nothing was created), and the status counts the refusal.
  let ReplyBody::Listed { volumes } = client.call(&RequestBody::List) else {
    strict.stop();
    panic!("a read is served while writes are refused");
  };
  assert!(volumes.is_empty(), "the refused create created nothing");
  let ReplyBody::DaemonStatus { report } = client.call(&RequestBody::DaemonStatus) else {
    strict.stop();
    panic!("status is served while writes are refused");
  };
  let counted: u64 = report
    .shards
    .iter()
    .flat_map(|shard| shard.refusals.iter())
    .filter(|refusal| refusal.kind == "durability_unmet")
    .map(|refusal| refusal.count)
    .sum();
  assert_eq!(
    counted, 1,
    "the refusal is counted under its own kind: {report:?}"
  );
  strict.stop();

  // Within the policy: a single failing host holds at most one of an f = 1 copyset's two copies, so the
  // loss under one failure is zero — within an ε of zero. The same fleet shape creates and snapshots.
  let (tolerant, tolerant_instance) = fleet_daemon(
    "durability-tolerant",
    Some(DurabilityBound {
      accepted_loss: 0.0,
      coincident_failures: 1,
    }),
  );
  let mut client = Client::connect(&tolerant_instance);
  let ReplyBody::Created { id } = client.call(&scratch("within")) else {
    tolerant.stop();
    panic!("a write within the declared durability is accepted");
  };
  let ReplyBody::Snapshotted { .. } = client.call(&RequestBody::Snapshot { volume: id }) else {
    tolerant.stop();
    panic!("a seal within the declared durability is accepted");
  };
  tolerant.stop();

  // No policy: accepted as before (the default — an accepted loss is the operator's to state, never derived).
  let (unpoliced, unpoliced_instance) = fleet_daemon("durability-none", None);
  let mut client = Client::connect(&unpoliced_instance);
  let ReplyBody::Created { .. } = client.call(&scratch("unpoliced")) else {
    unpoliced.stop();
    panic!("with no policy a write is accepted as before");
  };
  unpoliced.stop();
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
    version,
    ..
  } = client.call(&RequestBody::Attach {
    volume: id,
    snapshot: None,
    intent: Intent::Write,
    form: AttachRequest::Root,
  })
  else {
    panic!("attach");
  };
  assert_eq!(lease_epoch, Some(1));
  assert_eq!(path, None, "no bridge yet");
  assert_eq!(version, None, "a plain volume pins no green version");
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
  let created = client.call_as(id, &scratch("once"));
  let ReplyBody::Created { id: volume } = created else {
    panic!("create: {created:?}");
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
  let stale = client.call_as(id, &scratch("once"));
  assert!(
    matches!(
      stale,
      ReplyBody::Refused {
        refusal: Refusal::DuplicateRequest
      }
    ),
    "a retry of an acknowledged request is a stale duplicate, got {stale:?} (the original volume was {volume:?})"
  );
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
    form: AttachRequest::Root,
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
    form: AttachRequest::Root,
  }) else {
    panic!("attach read");
  };
  assert_eq!(lease_epoch, None);
  let ReplyBody::Attached { lease_epoch, .. } = b.call(&RequestBody::Attach {
    volume: id,
    snapshot: None,
    intent: Intent::Write,
    form: AttachRequest::Root,
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

/// A landing target on the host: a directory named with the process id (`mktemp -d`), owned by this
/// user — what `OsLand::open_target` admits — and removed when dropped, even on a failed assertion
/// (tests write only to a temp directory they name and remove, CLAUDE.md §4).
struct TargetDir {
  path: String,
}

impl Drop for TargetDir {
  fn drop(&mut self) {
    let _ = std::process::Command::new("rm")
      .args(["-rf", &self.path])
      .output();
  }
}

fn target_dir() -> TargetDir {
  let out = std::process::Command::new("mktemp")
    .args(["-d", "-t", &format!("slates-grant-{}", std::process::id())])
    .output()
    .unwrap();
  assert!(out.status.success(), "mktemp -d");
  let made = String::from_utf8_lossy(&out.stdout).trim().to_owned();
  // The canonical path: a landing target is resolved component by component with `O_NOFOLLOW` (§4.13 —
  // no symlink in the chain may redirect a write), and macOS's `/var` is a symlink to `/private/var`, so
  // the path `mktemp` prints would be refused `NotDirectory` at the link; the landing is given the real
  // directory. A read of the path, not a write.
  let path = std::fs::canonicalize(&made)
    .map(|p| p.to_string_lossy().into_owned())
    .unwrap_or(made);
  TargetDir { path }
}

/// AC-5.10 / T-5.12 and AC-2.8 (§4.13 "Grants"): a grant is accepted only with a **verified proof of
/// issuer authority** bound to the exact presented landing, never by the channel it arrives on. A
/// landing is presented (`GrantRequired`); an approval forged from the agent's own channel — the right
/// landing and manifest, a proof under a secret the caller does not hold — is refused
/// `GrantIssuerUnverified` and counted; the human surface, which maps the anchor and so holds the
/// daemon's issuer secret, proves the unchanged manifest and the grant issues; the landing then runs under
/// that grant and only its granted effect lands; `grants` lists it. Non-vacuous: the forged and the
/// genuine approval differ only in the secret behind the proof.
fn grant_scenario() {
  let (daemon, instance) = daemon("grant");
  let target = target_dir();
  let mut client = Client::connect(&instance);
  let (id, snapshot, landing, manifest) = present_landing(&mut client, &target.path);
  forged_approval_is_refused(&mut client, landing, manifest);
  let secret = daemon.segment().issuer_secret().unwrap();
  let grant = verified_approval_issues(&mut client, &secret, landing, manifest);
  // The landing runs under its grant, and `grants` lists it — across shards (the grant record lives on
  // the volume's owner shard, not necessarily the client's).
  assert!(matches!(
    client.call(&RequestBody::Land {
      volume: id,
      snapshot: Some(snapshot),
      target: target.path.clone(),
      filter: Filter::default(),
      grant: Some(grant),
    }),
    ReplyBody::Landed { .. }
  ));
  assert!(matches!(
    client.call(&RequestBody::Grants),
    ReplyBody::Grants { grants } if grants.len() == 1 && grants[0].id == grant
  ));
  daemon.stop();
}

/// A grant request for `landing` under `proof`, scoped once for the test's deadline.
fn grant_request(landing: u64, manifest: [u8; 32], proof: [u8; 32]) -> RequestBody {
  RequestBody::Grant {
    landing,
    manifest,
    scope: GrantScope::Once,
    term_ns: DEADLINE_NS,
    proof,
  }
}

/// Creates a volume, snapshots it, and presents its landing into `target`: the plan is computed, no
/// grant covers it, nothing is written. Returns the volume, the snapshot, the landing id and the
/// manifest the human must approve.
fn present_landing(
  client: &mut Client,
  target: &str,
) -> (
  slates_ipc::protocol::VolumeId,
  slates_ipc::protocol::SnapshotId,
  u64,
  [u8; 32],
) {
  let ReplyBody::Created { id } = client.call(&scratch("granted")) else {
    panic!("create");
  };
  let ReplyBody::Snapshotted { id: snapshot } = client.call(&RequestBody::Snapshot { volume: id })
  else {
    panic!("snapshot");
  };
  let presented = client.call(&RequestBody::Land {
    volume: id,
    snapshot: Some(snapshot),
    target: target.to_owned(),
    filter: Filter::default(),
    grant: None,
  });
  let ReplyBody::GrantRequired {
    landing, manifest, ..
  } = presented
  else {
    panic!("the landing was not presented: {presented:?}");
  };
  (id, snapshot, landing, manifest)
}

/// A forged approval — the agent knows the landing and its manifest but not the issuer secret — is
/// refused as unverified authority and issues nothing.
fn forged_approval_is_refused(client: &mut Client, landing: u64, manifest: [u8; 32]) {
  let forged = slates_server::landing::grant_proof(
    &[0u8; 32],
    landing,
    &manifest,
    GrantScope::Once,
    DEADLINE_NS,
  );
  let refused = client.call(&grant_request(landing, manifest, forged));
  assert!(
    matches!(
      refused,
      ReplyBody::Refused {
        refusal: Refusal::GrantIssuerUnverified
      }
    ),
    "a forged approval is refused as unverified authority, got {refused:?}"
  );
  assert!(
    matches!(
      client.call(&RequestBody::Grants),
      ReplyBody::Grants { grants } if grants.is_empty()
    ),
    "a forged approval issued nothing"
  );
}

/// The human surface — it maps the anchor segment and holds the secret the daemon minted — proves the
/// unchanged manifest and the grant issues; the same proof over a modified plan does not verify.
fn verified_approval_issues(
  client: &mut Client,
  secret: &[u8; 32],
  landing: u64,
  manifest: [u8; 32],
) -> u64 {
  let proof =
    slates_server::landing::grant_proof(secret, landing, &manifest, GrantScope::Once, DEADLINE_NS);
  let ReplyBody::Granted { grant } = client.call(&grant_request(landing, manifest, proof)) else {
    panic!("the verified approval did not issue a grant");
  };
  let mut other = manifest;
  other[0] ^= 1;
  assert!(matches!(
    client.call(&grant_request(landing, other, proof)),
    ReplyBody::Refused {
      refusal: Refusal::GrantIssuerUnverified
    }
  ));
  grant
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

/// AC-5.10 / T-5.12, AC-2.8: a human's grant is authenticated by a verified proof of issuer authority
/// bound to the exact presented landing — a forged or modified-plan approval refuses before writing, the
/// enrolled human surface's approval issues and lands. Its own test so it is observable on its own.
#[test]
fn a_grant_is_accepted_only_with_a_verified_proof_bound_to_the_presented_landing() {
  grant_scenario();
}

/// AC-2.13 / T-2.15 (§4.13 "Principals", "Access lists"): distinct consumers sharing a uid cannot use
/// each other's VFS or grant rights. The human surface enrolls a consumer under the account; a workload
/// binds its channel to it with the capability (a forged capability refuses `ConsumerNotEnrolled`); the
/// consumer sees nothing of the account's volume until the owner shares it (`Forbidden` before any
/// lookup), then only the right shared (read, not write); a workload cannot mint enrollment authority
/// (`GrantIssuerUnverified`); once the human revokes the consumer, its very next verb refuses
/// `ConsumerRevoked` before any effect. Non-vacuous: the forged and the genuine capability differ only in
/// the secret, and the shared and the unshared right differ only in the access entry. The daemon runs
/// [`TEST_SHARDS`] shards and each client lands on its own, so the consumer's record is enrolled on one
/// partition and attested from a channel on another — the routing an in-partition lookup cannot do.
#[test]
fn distinct_consumers_under_one_uid_hold_only_the_rights_shared_with_them_until_revoked() {
  let (daemon, instance) = daemon("consumers");
  let secret = daemon.segment().issuer_secret().unwrap();
  let account = current_uid();
  let mut owner = Client::connect(&instance);
  let mut workload = Client::connect(&instance);

  a_workload_cannot_mint_enrollment_authority(&mut workload, account);
  let (consumer, capability) = enroll(&mut owner, &secret, account);
  only_the_genuine_capability_binds_the_channel(&mut workload, consumer, &capability);
  let id = the_consumer_holds_only_the_right_shared(&mut owner, &mut workload, account, consumer);
  once_revoked_every_verb_refuses_before_any_effect(
    &mut owner,
    &mut workload,
    &secret,
    consumer,
    id,
  );
  daemon.stop();
}

/// A forged proof of issuer authority refuses: an agent cannot enroll itself.
fn a_workload_cannot_mint_enrollment_authority(workload: &mut Client, account: u32) {
  let forged = slates_server::landing::enroll_proof(&[0u8; 32], account);
  assert!(matches!(
    workload.call(&RequestBody::Enroll {
      account,
      proof: forged
    }),
    ReplyBody::Refused {
      refusal: Refusal::GrantIssuerUnverified
    }
  ));
}

/// A forged capability does not bind the channel; the genuine one does, and the channel's principal is
/// then the consumer.
fn only_the_genuine_capability_binds_the_channel(
  workload: &mut Client,
  consumer: u64,
  capability: &[u8; 32],
) {
  let wrong = slates_server::landing::attest_proof(&[0u8; 32], workload.client);
  assert!(matches!(
    workload.call(&RequestBody::Attest {
      consumer,
      proof: wrong
    }),
    ReplyBody::Refused {
      refusal: Refusal::ConsumerNotEnrolled
    }
  ));
  let proof = slates_server::landing::attest_proof(capability, workload.client);
  let attested = workload.call(&RequestBody::Attest { consumer, proof });
  assert!(
    matches!(attested, ReplyBody::Attested),
    "the genuine capability binds the channel, got {attested:?}"
  );
}

/// The account owns a volume the consumer, sharing the uid, sees none of until shared; shared read only,
/// the consumer can read its status and not snapshot it. Returns the volume.
fn the_consumer_holds_only_the_right_shared(
  owner: &mut Client,
  workload: &mut Client,
  account: u32,
  consumer: u64,
) -> VolumeId {
  let ReplyBody::Created { id } = owner.call(&scratch("private")) else {
    panic!("create");
  };
  assert!(matches!(
    workload.call(&RequestBody::Status { volume: id }),
    ReplyBody::Refused {
      refusal: Refusal::Forbidden { .. }
    }
  ));
  assert!(matches!(
    owner.call(&RequestBody::Share {
      volume: id,
      principal: slates_ipc::protocol::Principal::Consumer { account, consumer },
      rights: slates_ipc::protocol::Rights {
        read: true,
        write: false,
        admin: false
      },
    }),
    ReplyBody::Shared
  ));
  assert!(matches!(
    workload.call(&RequestBody::Status { volume: id }),
    ReplyBody::Status { .. }
  ));
  assert!(matches!(
    workload.call(&RequestBody::Snapshot { volume: id }),
    ReplyBody::Refused {
      refusal: Refusal::Forbidden { .. }
    }
  ));
  id
}

/// The human revokes the consumer: its next verb refuses before any effect, and stays refused.
fn once_revoked_every_verb_refuses_before_any_effect(
  owner: &mut Client,
  workload: &mut Client,
  secret: &[u8; 32],
  consumer: u64,
  id: VolumeId,
) {
  let revoke = slates_server::landing::revoke_proof(secret, consumer);
  assert!(matches!(
    owner.call(&RequestBody::Revoke {
      consumer,
      proof: revoke
    }),
    ReplyBody::Revoked
  ));
  assert!(matches!(
    workload.call(&RequestBody::Status { volume: id }),
    ReplyBody::Refused {
      refusal: Refusal::ConsumerRevoked
    }
  ));
  assert!(matches!(
    workload.call(&RequestBody::List),
    ReplyBody::Refused {
      refusal: Refusal::ConsumerRevoked
    }
  ));
}

/// The human surface enrolls a consumer under `account`, proving issuer authority with the daemon's
/// secret; returns the consumer id and the capability shown once.
fn enroll(client: &mut Client, secret: &[u8; 32], account: u32) -> (u64, [u8; 32]) {
  let proof = slates_server::landing::enroll_proof(secret, account);
  let ReplyBody::Enrolled { consumer, secret } =
    client.call(&RequestBody::Enroll { account, proof })
  else {
    panic!("the human surface's enrollment was refused");
  };
  (consumer, secret)
}

/// This process's uid — the account every client of these tests rendezvouses as.
fn current_uid() -> u32 {
  rustix::process::getuid().as_raw()
}

/// §4.15 the clean-file digest over the wire (AC-1.17 / T-1.21): over an overlay of this crate's
/// source tree, the digest of an untouched base file names exactly the bytes `read_base` returns,
/// two exports encode to the same bytes (the determinism gate on the wire), a pinned (witnessed)
/// entry refuses the typed `DigestNotClean` rather than a stale digest, and a scratch volume has
/// no base to digest.
fn digest_scenario() {
  let (daemon, instance) = daemon("digest");
  let mut client = Client::connect(&instance);
  let base = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
  let ReplyBody::Created { id: overlay } = client.call(&RequestBody::Create {
    name: "digest-over".into(),
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
  let digest = RequestBody::Digest {
    volume: overlay,
    path: "/lib.rs".into(),
  };
  let ReplyBody::Digest { identity, size } = client.call(&digest) else {
    panic!("digest");
  };
  assert_eq!(identity, *blake3::hash(&bytes).as_bytes());
  assert_eq!(size, u64::try_from(bytes.len()).unwrap());
  assert_eq!(
    slates_ipc::protocol::encode_body(&client.call(&digest)),
    slates_ipc::protocol::encode_body(&ReplyBody::Digest { identity, size }),
    "two exports of unchanged content are byte-identical on the wire"
  );
  assert!(matches!(
    client.call(&RequestBody::Pin {
      volume: overlay,
      paths: Some(vec!["/lib.rs".into()])
    }),
    ReplyBody::Pinned { entries: 1 }
  ));
  assert!(
    matches!(
      client.call(&digest),
      ReplyBody::Refused {
        refusal: Refusal::DigestNotClean
      }
    ),
    "a pinned entry is witnessed, so it is no longer clean: typed, never stale"
  );
  let ReplyBody::Created { id: scratch_id } = client.call(&scratch("digest-scratch")) else {
    panic!("create scratch");
  };
  assert!(matches!(
    client.call(&RequestBody::Digest {
      volume: scratch_id,
      path: "/x".into()
    }),
    ReplyBody::Refused {
      refusal: Refusal::Unsupported { .. }
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
  landing_refusal_scenario();
  bulk_and_overlay_scenario();
  digest_scenario();
  // §4.2/§4.5 version-slab reservation scenarios run here, one daemon at a time, for the same reason.
  version_reservation_scenario();
  version_reservation_moves_on_resize_scenario();
  version_stats_scenario();
  telemetry_scenario();
  snapshot_destroy_scenario();
  green_chain_scenario();
  merge_submit_scenario();
  merge_modify_scenario();
  merge_lagging_scenario();
  merge_rebase_scenario();
  merge_declare_scenario();
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

/// The shard's version-slab reservation is observable in the daemon status (§4.2 "statfs includes
/// backed inode availability", at the shard level): the reported slab capacity is the configured
/// one, and committed version slots move as a volume's inode allowance is reserved.
fn version_stats_scenario() {
  let (daemon, instance) = capped_daemon("version-stats", SMALL_VERSION_SLAB);
  let mut client = Client::connect(&instance);
  let stats = |client: &mut Client| -> (u64, u64) {
    let ReplyBody::DaemonStatus { report } = client.call(&RequestBody::DaemonStatus) else {
      panic!("daemon status");
    };
    let slots = report
      .shards
      .iter()
      .map(|s| s.version_slots)
      .max()
      .unwrap_or(0);
    let committed = report.shards.iter().map(|s| s.committed_versions).sum();
    (slots, committed)
  };
  let (slots, before) = stats(&mut client);
  assert_eq!(
    slots,
    u64::try_from(SMALL_VERSION_SLAB).unwrap(),
    "the reported version slab is the configured capacity"
  );
  assert_eq!(before, 0, "no volumes yet, nothing committed to the slab");
  let ReplyBody::Created { .. } = client.call(&RequestBody::Create {
    name: "vol".into(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }) else {
    panic!("create");
  };
  let (_, after) = stats(&mut client);
  assert!(
    after > before,
    "a volume's inode allowance is committed against the version slab: {after}"
  );
  daemon.stop();
}

/// Shape: verbs between two acknowledgements in the overflow loop, so the completion records a client
/// leaves unacknowledged stay a small window while the ring fills (the raw test client never
/// acknowledges on its own).
const ACK_EVERY: usize = 32;

/// One request's spans are one trace with their causes named, the trace distinct from the request
/// identity; a chokepoint no producer exercises here reports typed absence; and overflowing a shard's
/// ring is reported as a typed loss marker, never a silent gap — followed by use through the
/// `Telemetry` drain the status surfaces read (§4.14 three-id law; AC-0.11/T-0.11 "drop a producer and
/// overflow telemetry"). Do: run one `Create`, drain both shards' rings, follow the request's id through
/// the spans. Expect: its `ring.request` is a root on the client's shard; its `shard.op` — on the owner,
/// which may be the other shard — is caused by that ring span and shares its trace; its `log.append`
/// is caused by the `shard.op` and lies within it; the trace is not the request word widened;
/// `shard.op` is fresh on the owner while `archive.chunk`, `ship.record` and `consensus.step` are
/// absent/unknown with no producer on this laptop. Then run more verbs than a ring holds without
/// draining. Expect: the owner's next drain marks `shed_before > 0` and `dropped_total > 0`, carries at
/// most the reply quota, and the batches until `remaining` is 0 add up to what the ring held.
fn telemetry_scenario() {
  let profile = profile();
  let instance = format!("srv-telemetry-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  let ring_capacity = usize::try_from(config.region.slots).unwrap();
  let quota = config.telemetry_spans_per_reply;
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: "slates-seg-telemetry".to_owned(),
    },
  )
  .unwrap();
  let mut client = Client::connect(&instance);

  // One verb, then both rings drained: the request's spans across the shards, by its identity.
  let created = RequestId {
    client: client.client,
    sequence: client.sequence + 1,
  };
  let ReplyBody::Created { id } = client.call(&scratch("tele-one")) else {
    panic!("create");
  };
  let batches: Vec<TelemetryReport> = (0..TEST_SHARDS)
    .map(|partition| drain(&mut client, partition))
    .collect();
  let owner = assert_one_trace_with_its_causes(&batches, created);
  assert_laptop_absences(&batches);
  overflow_and_drain(&mut client, id, owner, ring_capacity, quota);
  daemon.stop();
}

/// Drains one shard's telemetry ring.
fn drain(client: &mut Client, partition: u16) -> TelemetryReport {
  let ReplyBody::Telemetry { report } = client.call(&RequestBody::Telemetry { partition }) else {
    panic!("telemetry drain of shard {partition}");
  };
  report
}

/// A span's chokepoint name, through its batch's registry entries.
fn point_of(batch: &TelemetryReport, span: &SpanRecord) -> String {
  batch.chokepoints[usize::try_from(span.point).unwrap()]
    .name
    .clone()
}

/// The request's spans across the drained batches, each with the batch it came from.
fn spans_of(
  batches: &[TelemetryReport],
  request: RequestId,
) -> Vec<(&TelemetryReport, &SpanRecord)> {
  batches
    .iter()
    .flat_map(|batch| {
      batch
        .spans
        .iter()
        .filter(|span| {
          span.request_client == request.client && span.request_sequence == request.sequence
        })
        .map(move |span| (batch, span))
    })
    .collect()
}

/// Follows `created` through the chokepoints it crossed (§4.14 three-id law): `ring.request` is a
/// root; `shard.op` (on the owner, possibly the other shard) is caused by it and shares its trace;
/// `log.append` is caused by `shard.op` and lies within it; the trace is not the request word; three
/// distinct span ids; `shard.op` fresh and expected on the owner. Returns the owner's partition.
fn assert_one_trace_with_its_causes(batches: &[TelemetryReport], created: RequestId) -> u16 {
  let mine = spans_of(batches, created);
  let find = |name: &str| -> (&TelemetryReport, &SpanRecord) {
    *mine
      .iter()
      .find(|(batch, span)| point_of(batch, span) == name)
      .unwrap_or_else(|| panic!("the create's {name} span was drained: {mine:?}"))
  };
  let (ring_shard, ring) = find("ring.request");
  let (op_shard, op) = find("shard.op");
  let (append_shard, append) = find("log.append");
  assert_eq!(
    ring.cause,
    CauseRecord::Root,
    "the slot read opens the trace"
  );
  assert_eq!(
    op.cause,
    CauseRecord::Span { id: ring.span },
    "the verb is caused by the ring read, across the shard boundary when the owner is the other shard (ring on shard {}, verb on shard {})",
    ring_shard.partition,
    op_shard.partition
  );
  assert_eq!(
    append.cause,
    CauseRecord::Span { id: op.span },
    "the log append is caused by the verb"
  );
  assert_one_trace_distinct_from_the_request(ring, op, append, created);
  assert_eq!(
    op_shard.partition, append_shard.partition,
    "the append is on the verb's shard"
  );
  assert!(
    op.start_ns <= append.start_ns && append.end_ns <= op.end_ns,
    "the append lies within the verb on the one clock they share"
  );
  let op_entry = op_shard
    .chokepoints
    .iter()
    .find(|c| c.name == "shard.op")
    .unwrap();
  assert!(op_entry.fresh && op_entry.spans >= 1 && op_entry.expected);
  assert!(op_entry.latest_age_ns.is_some());
  op_shard.partition
}

/// The three spans share one trace that is not the request word widened, under three distinct span
/// ids (§4.14 three-id law: the identities are distinct in value, not only in type).
fn assert_one_trace_distinct_from_the_request(
  ring: &SpanRecord,
  op: &SpanRecord,
  append: &SpanRecord,
  created: RequestId,
) {
  let trace = (ring.trace_high, ring.trace_low);
  assert_eq!(
    (op.trace_high, op.trace_low),
    trace,
    "one trace for the request"
  );
  assert_eq!((append.trace_high, append.trace_low), trace);
  assert_ne!(
    trace,
    (0, created.word()),
    "the trace is its own identity, not the request word widened"
  );
  let mut ids = vec![ring.span, op.span, append.span];
  ids.sort_unstable();
  ids.dedup();
  assert_eq!(ids.len(), 3, "three distinct span ids");
}

/// Every batch carries the whole registry; the chokepoints no producer exercises on a laptop are typed
/// absent — not fresh, no age, `unknown`, not expected — and same-node forwards carry their cause.
fn assert_laptop_absences(batches: &[TelemetryReport]) {
  for batch in batches {
    assert_eq!(
      batch.chokepoints.len(),
      9,
      "the whole registry, every batch"
    );
    for name in ["archive.chunk", "ship.record", "consensus.step"] {
      let entry = batch.chokepoints.iter().find(|c| c.name == name).unwrap();
      assert!(
        !entry.fresh,
        "{name} has no producer on a laptop: not fresh"
      );
      assert_eq!(
        entry.latest_age_ns, None,
        "{name} never reported: no age, not a zero"
      );
      assert_eq!(entry.absence, AbsenceIs::Unknown, "{name}: typed absence");
      assert!(!entry.expected, "{name}: no producer runs here");
    }
    assert_eq!(
      batch.missing_links, 0,
      "same-node forwards carry their cause"
    );
  }
}

/// Overflow: more verbs than a ring holds, undrained — the owner shard of `id` gets two spans per
/// status, so `ring_capacity` calls shed at least `ring_capacity` spans there. Expect: the next drain
/// marks the loss (`shed_before`, `dropped_total`), carries at most the reply quota, and the batches
/// until `remaining` is 0 add up to what the ring held, with no loss between back-to-back drains.
fn overflow_and_drain(
  client: &mut Client,
  id: slates_ipc::protocol::VolumeId,
  owner: u16,
  ring_capacity: usize,
  quota: usize,
) {
  for count in 1..=ring_capacity {
    let ReplyBody::Status { .. } = client.call(&RequestBody::Status { volume: id }) else {
      panic!("status");
    };
    if count % ACK_EVERY == 0 {
      let up_to = client.sequence;
      let ReplyBody::Acknowledged = client.call(&RequestBody::Acknowledge { up_to }) else {
        panic!("acknowledge");
      };
    }
  }
  let first = drain(client, owner);
  assert!(
    first.shed_before > 0,
    "the ring shed spans before this batch and says so (capacity {ring_capacity}): {first:?}"
  );
  assert!(first.dropped_total >= first.shed_before);
  assert!(
    first.spans.len() <= quota,
    "a batch never exceeds the reply quota {quota}"
  );
  // The batches until the ring is empty add up to what it held at the first drain; no batch after the
  // first carries loss (none happened between the drains).
  let held = first.spans.len() + usize::try_from(first.remaining).unwrap();
  let mut drained = first.spans.len();
  let mut remaining = first.remaining;
  let mut rounds = 0usize;
  while remaining > 0 {
    let more = drain(client, owner);
    assert_eq!(more.shed_before, 0, "no loss between back-to-back drains");
    drained += more.spans.len();
    remaining = more.remaining;
    rounds += 1;
    assert!(rounds <= held / quota.max(1) + 2, "the drain converges");
  }
  // The drains are verbs on the owner too, each leaving its own spans in the ring after draining it
  // (never more than one verb's spans between two drains), so the total is the held count plus those.
  assert!(
    drained >= held && drained <= held + rounds * 3,
    "the batches add up to what the ring held (held {held}, drained {drained}, rounds {rounds})"
  );
  println!(
    "telemetry: ring capacity {ring_capacity} spans, reply quota {quota}; after {ring_capacity} status verbs the owner shard {owner} shed {} (dropped_total {}), held {held}, drained {drained} over {} batches",
    first.shed_before,
    first.dropped_total,
    rounds + 1
  );
}

/// Destroying a snapshot over the wire, and the clone pin around it (§4.5/§4.2): a snapshot a clone
/// was made from cannot be destroyed while the clone lives, and can once the clone's destroy
/// completes — proof the server releases the origin's pin (`unpin`) when a clone is torn down.
fn snapshot_destroy_scenario() {
  let (daemon, instance) = daemon("snap-destroy");
  let mut client = Client::connect(&instance);
  let create = |n: &str| RequestBody::Create {
    name: n.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  };
  let ReplyBody::Created { id } = client.call(&create("origin")) else {
    panic!("create");
  };
  let ReplyBody::Snapshotted { id: snap } = client.call(&RequestBody::Snapshot { volume: id })
  else {
    panic!("snapshot");
  };
  let ReplyBody::Cloned { id: clone } = client.call(&RequestBody::Clone {
    volume: id,
    snapshot: snap,
    name: "clone".into(),
  }) else {
    panic!("clone");
  };
  // The clone pins the snapshot it was made from: destroying that snapshot is refused.
  assert!(
    matches!(
      client.call(&RequestBody::DestroySnapshot {
        volume: id,
        snapshot: snap
      }),
      ReplyBody::Refused { .. }
    ),
    "a live clone pins the snapshot"
  );
  // Destroy the clone and wait for its destroy to complete; that releases the pin.
  assert!(matches!(
    client.call(&RequestBody::Destroy { volume: clone }),
    ReplyBody::Destroyed
  ));
  let started = Instant::now();
  loop {
    let ReplyBody::Listed { volumes } = client.call(&RequestBody::List) else {
      panic!("list");
    };
    if volumes.iter().all(|v| v.name != "clone") {
      break;
    }
    assert!(started.elapsed() < CREDIT_WAIT, "clone destroy completes");
  }
  // With the pin released, the snapshot can now be destroyed.
  assert!(
    matches!(
      client.call(&RequestBody::DestroySnapshot {
        volume: id,
        snapshot: snap
      }),
      ReplyBody::SnapshotDestroyed
    ),
    "the pin released by the clone's destroy, the snapshot is destroyed"
  );
  daemon.stop();
}

/// The merge chain's read side (§4.16 Phase 6 Task 1): a fresh green volume exists and reports
/// version 0. The green's owner shard holds its merge engine; `versions` routes to it.
fn green_chain_scenario() {
  let (daemon, instance) = daemon("green");
  let mut client = Client::connect(&instance);
  let ReplyBody::GreenCreated { id } = client.call(&RequestBody::CreateGreen {
    name: "g".to_owned(),
    require_evidence: false,
    base: None,
  }) else {
    panic!("create green");
  };
  // A duplicate name is refused, like any volume.
  assert!(matches!(
    client.call(&RequestBody::CreateGreen {
      name: "g".to_owned(),
      require_evidence: false,
      base: None,
    }),
    ReplyBody::Refused {
      refusal: Refusal::AlreadyExists { .. }
    }
  ));
  let ReplyBody::Versions { head } = client.call(&RequestBody::Versions { green: id }) else {
    panic!("versions");
  };
  assert_eq!(head, 0, "a fresh green is at version 0");
  daemon.stop();
}

/// The merge submit flow and the verdict (§4.16 Phase 6 Task 6): two agents create work volumes over
/// a green, both based on version 0. The first's edit merges on the fast path (the chain advances to
/// version 1); the second, still based on version 0, declared the same file the first just committed,
/// so its submit conflicts with a window rather than silently overwriting. The whole flow runs
/// through the real verbs on the green's owner shard. Non-vacuous: a broken verdict would accept both.
fn merge_submit_scenario() {
  let (daemon, instance) = daemon("merge");
  let mut client = Client::connect(&instance);
  let ReplyBody::GreenCreated { id: green } = client.call(&RequestBody::CreateGreen {
    name: "green".to_owned(),
    require_evidence: false,
    base: None,
  }) else {
    panic!("create green");
  };
  let mut work = |name: &str| -> slates_ipc::protocol::VolumeId {
    let ReplyBody::WorkCreated { id, base } = client.call(&RequestBody::CreateWork {
      green,
      name: name.to_owned(),
    }) else {
      panic!("create work");
    };
    assert_eq!(base, 0, "a work over a fresh green is based on version 0");
    id
  };
  let a = work("a");
  let b = work("b");
  let declare = |client: &mut Client, w, bytes: &[u8]| {
    assert!(matches!(
      client.call(&RequestBody::Edit {
        work: w,
        path: "f".to_owned(),
        at: 0,
        delete_len: 0,
        bytes: bytes.to_vec(),
      }),
      ReplyBody::Edited
    ));
  };
  declare(&mut client, a, b"hello");
  declare(&mut client, b, b"world");

  // A merges on the fast path; the chain advances.
  let ReplyBody::Submitted { version, conflicts } = client.call(&RequestBody::Submit {
    work: a,
    evidence: Vec::new(),
  }) else {
    panic!("submit a");
  };
  assert!(
    conflicts.is_empty(),
    "no conflict for the first submit: {conflicts:?}"
  );
  assert_eq!(version, Some(1), "A is accepted as version 1");
  let ReplyBody::Versions { head } = client.call(&RequestBody::Versions { green }) else {
    panic!("versions");
  };
  assert_eq!(head, 1, "the green advanced to version 1");

  // The chain's read side: the file A committed changed after version 0, but not after version 1.
  let ReplyBody::ChangedSince { paths } =
    client.call(&RequestBody::ChangedSince { green, version: 0 })
  else {
    panic!("changed_since");
  };
  assert_eq!(paths, vec!["f".to_owned()], "f changed after version 0");
  let ReplyBody::ChangedSince { paths } =
    client.call(&RequestBody::ChangedSince { green, version: 1 })
  else {
    panic!("changed_since");
  };
  assert!(
    paths.is_empty(),
    "nothing changed after the head: {paths:?}"
  );

  // B, still based on version 0, touched the same file — it conflicts rather than clobbering A.
  let ReplyBody::Submitted { version, conflicts } = client.call(&RequestBody::Submit {
    work: b,
    evidence: Vec::new(),
  }) else {
    panic!("submit b");
  };
  assert_eq!(version, None, "B is not accepted");
  assert!(
    conflicts.iter().any(|w| w.path == "f"),
    "B conflicts on the file A committed: {conflicts:?}"
  );
  daemon.stop();
}

/// Submitting against a non-empty green (§4.16): a work modifies a file the green already holds
/// (seeded from the green's content, derived against the green's current base), and it merges. Proof
/// that submit works past the fresh-green case — the current-base path, not just an empty base.
fn merge_modify_scenario() {
  let (daemon, instance) = daemon("merge-mod");
  let mut client = Client::connect(&instance);
  let ReplyBody::GreenCreated { id: green } = client.call(&RequestBody::CreateGreen {
    name: "g2".to_owned(),
    require_evidence: false,
    base: None,
  }) else {
    panic!("create green");
  };
  let edit = |client: &mut Client, work, at, delete_len, bytes: &[u8]| {
    assert!(matches!(
      client.call(&RequestBody::Edit {
        work,
        path: "f".to_owned(),
        at,
        delete_len,
        bytes: bytes.to_vec(),
      }),
      ReplyBody::Edited
    ));
  };
  // Seed the green with f = "hello".
  let ReplyBody::WorkCreated { id: seed, .. } = client.call(&RequestBody::CreateWork {
    green,
    name: "seed".to_owned(),
  }) else {
    panic!("create work");
  };
  edit(&mut client, seed, 0, 0, b"hello");
  assert!(matches!(
    client.call(&RequestBody::Submit {
      work: seed,
      evidence: Vec::new()
    }),
    ReplyBody::Submitted {
      version: Some(1),
      ..
    }
  ));
  // A new work, based on version 1, overwrites f's bytes — it merges against the current base.
  let ReplyBody::WorkCreated { id: modw, base } = client.call(&RequestBody::CreateWork {
    green,
    name: "mod".to_owned(),
  }) else {
    panic!("create work");
  };
  assert_eq!(base, 1, "the work is based on the green's head, version 1");
  edit(&mut client, modw, 0, 5, b"world");
  let ReplyBody::Submitted { version, conflicts } = client.call(&RequestBody::Submit {
    work: modw,
    evidence: Vec::new(),
  }) else {
    panic!("submit");
  };
  assert!(
    conflicts.is_empty(),
    "the lone modify merges: {conflicts:?}"
  );
  assert_eq!(version, Some(2), "the green advanced to version 2");
  daemon.stop();
}

/// A work that lagged behind an intervening submit still merges when it touches other files (§4.16):
/// base_at reconstructs the green at the work's older base version, and the verdict's basis skips the
/// unchanged files. A lagging work on a disjoint file merges past the version it fell behind.
fn merge_lagging_scenario() {
  let (daemon, instance) = daemon("merge-lag");
  let mut client = Client::connect(&instance);
  let ReplyBody::GreenCreated { id: green } = client.call(&RequestBody::CreateGreen {
    name: "g3".to_owned(),
    require_evidence: false,
    base: None,
  }) else {
    panic!("create green");
  };
  let create_file = |client: &mut Client, work, name: &str, bytes: &[u8]| {
    assert!(matches!(
      client.call(&RequestBody::Edit {
        work,
        path: name.to_owned(),
        at: 0,
        delete_len: 0,
        bytes: bytes.to_vec(),
      }),
      ReplyBody::Edited
    ));
  };
  let work = |client: &mut Client, name: &str| -> (slates_ipc::protocol::VolumeId, u64) {
    let ReplyBody::WorkCreated { id, base } = client.call(&RequestBody::CreateWork {
      green,
      name: name.to_owned(),
    }) else {
      panic!("create work");
    };
    (id, base)
  };
  // Seed the green with "base" so later works are based on version 1, not 0.
  let (seed, _) = work(&mut client, "seed");
  create_file(&mut client, seed, "base", b"x");
  assert!(matches!(
    client.call(&RequestBody::Submit {
      work: seed,
      evidence: Vec::new()
    }),
    ReplyBody::Submitted {
      version: Some(1),
      ..
    }
  ));
  // A and B both start at version 1.
  let (a, base_a) = work(&mut client, "a");
  let (b, base_b) = work(&mut client, "b");
  assert_eq!((base_a, base_b), (1, 1), "both based on version 1");
  create_file(&mut client, a, "a", b"a-content");
  create_file(&mut client, b, "b", b"b-content");
  // B submits first, advancing the green to version 2; A now lags at base 1.
  assert!(matches!(
    client.call(&RequestBody::Submit {
      work: b,
      evidence: Vec::new()
    }),
    ReplyBody::Submitted {
      version: Some(2),
      ..
    }
  ));
  // A, based on version 1, touched a different file — it merges past version 2 as version 3.
  let ReplyBody::Submitted { version, conflicts } = client.call(&RequestBody::Submit {
    work: a,
    evidence: Vec::new(),
  }) else {
    panic!("submit a");
  };
  assert!(
    conflicts.is_empty(),
    "a disjoint lagging work merges: {conflicts:?}"
  );
  assert_eq!(version, Some(3), "A merged past the version it fell behind");
  daemon.stop();
}

/// Creates a work over `green`, returning its id.
fn work_over(
  client: &mut Client,
  green: slates_ipc::protocol::VolumeId,
  name: &str,
) -> slates_ipc::protocol::VolumeId {
  let ReplyBody::WorkCreated { id, .. } = client.call(&RequestBody::CreateWork {
    green,
    name: name.to_owned(),
  }) else {
    panic!("create work");
  };
  id
}

/// Declares a splice on `work`'s file `f`.
fn edit_f(
  client: &mut Client,
  work: slates_ipc::protocol::VolumeId,
  at: u64,
  del: u64,
  bytes: &[u8],
) {
  assert!(matches!(
    client.call(&RequestBody::Edit {
      work,
      path: "f".to_owned(),
      at,
      delete_len: del,
      bytes: bytes.to_vec(),
    }),
    ReplyBody::Edited
  ));
}

/// The green's head version.
fn green_head(client: &mut Client, green: slates_ipc::protocol::VolumeId) -> u64 {
  let ReplyBody::Versions { head } = client.call(&RequestBody::Versions { green }) else {
    panic!("versions");
  };
  head
}

/// A clean rebase commits nothing to the green, then the moved work submits (§4.16). A tail edit
/// based on version 1 rebases onto the head (2) — the head stays 2 — then submits as version 3.
fn rebase_clean_part(client: &mut Client, green: slates_ipc::protocol::VolumeId) {
  let tail = work_over(client, green, "tail");
  edit_f(client, tail, 8, 2, b"YY");
  let ReplyBody::Rebased { version, conflicts } = client.call(&RequestBody::Rebase { work: tail })
  else {
    panic!("rebase");
  };
  assert!(
    conflicts.is_empty(),
    "the clean tail edit rebases: {conflicts:?}"
  );
  assert_eq!(version, Some(2), "the work is rebased onto the head");
  assert_eq!(green_head(client, green), 2, "a rebase commits nothing");

  let ReplyBody::Submitted { version, conflicts } = client.call(&RequestBody::Submit {
    work: tail,
    evidence: Vec::new(),
  }) else {
    panic!("submit");
  };
  assert!(
    conflicts.is_empty(),
    "the rebased work submits: {conflicts:?}"
  );
  assert_eq!(version, Some(3), "the rebased work advances the green to 3");
  assert_eq!(green_head(client, green), 3);
}

/// A rebase that conflicts returns the windows and commits nothing (§4.16). Two works based on 3
/// overwrite the same region; one submits (4), the other's rebase conflicts and the head stays 4.
fn rebase_conflict_part(client: &mut Client, green: slates_ipc::protocol::VolumeId) {
  let winner = work_over(client, green, "winner");
  let loser = work_over(client, green, "loser");
  edit_f(client, winner, 0, 2, b"PP");
  edit_f(client, loser, 0, 2, b"QQ");
  assert!(matches!(
    client.call(&RequestBody::Submit {
      work: winner,
      evidence: Vec::new()
    }),
    ReplyBody::Submitted {
      version: Some(4),
      ..
    }
  ));
  let ReplyBody::Rebased { version, conflicts } = client.call(&RequestBody::Rebase { work: loser })
  else {
    panic!("rebase loser");
  };
  assert_eq!(version, None, "the overlapping work does not rebase");
  assert!(
    conflicts.iter().any(|w| w.path == "f"),
    "the conflict names the file: {conflicts:?}"
  );
  assert_eq!(
    green_head(client, green),
    4,
    "a conflicting rebase commits nothing"
  );
}

/// Rebase, the corrective path (§4.16), over the wire: a work whose operations map cleanly onto the
/// head is rebased — the green is committed nothing (its head does not move), the work is moved onto
/// the head, and a following submit accepts. A work that conflicts on rebase gets the windows and,
/// again, the green does not move. Non-vacuous: were rebase to commit, the head would jump; were it
/// to move the work wrongly, the following submit would not reach the expected version.
fn merge_rebase_scenario() {
  let (daemon, instance) = daemon("merge-rebase");
  let mut client = Client::connect(&instance);
  let ReplyBody::GreenCreated { id: green } = client.call(&RequestBody::CreateGreen {
    name: "g5".to_owned(),
    require_evidence: false,
    base: None,
  }) else {
    panic!("create green");
  };

  // Seed f, then move the head under a work by inserting at the front.
  let seed = work_over(&mut client, green, "seed");
  edit_f(&mut client, seed, 0, 0, b"0123456789");
  assert!(matches!(
    client.call(&RequestBody::Submit {
      work: seed,
      evidence: Vec::new()
    }),
    ReplyBody::Submitted {
      version: Some(1),
      ..
    }
  ));
  let front = work_over(&mut client, green, "front");
  edit_f(&mut client, front, 0, 0, b"AB");
  assert!(matches!(
    client.call(&RequestBody::Submit {
      work: front,
      evidence: Vec::new()
    }),
    ReplyBody::Submitted {
      version: Some(2),
      ..
    }
  ));

  rebase_clean_part(&mut client, green);
  rebase_conflict_part(&mut client, green);
  daemon.stop();
}

/// Declares a namespace or metadata operation on a work, asserting it is recorded.
fn declare(
  client: &mut Client,
  work: slates_ipc::protocol::VolumeId,
  op: slates_ipc::protocol::WorkOp,
) {
  assert!(matches!(
    client.call(&RequestBody::Declare { work, op }),
    ReplyBody::Declared
  ));
}

/// A work declares directory, symlink and mode operations in one increment, all against a file the
/// green already holds; they merge as one version (§4.16 — every dimension the deriver composes).
fn declare_metadata_part(client: &mut Client, green: slates_ipc::protocol::VolumeId) {
  use slates_ipc::protocol::WorkOp;
  let meta = work_over(client, green, "meta");
  declare(
    client,
    meta,
    WorkOp::Mkdir {
      path: "d".to_owned(),
    },
  );
  declare(
    client,
    meta,
    WorkOp::Symlink {
      path: "l".to_owned(),
      target: "f".to_owned(),
    },
  );
  declare(
    client,
    meta,
    WorkOp::SetMode {
      path: "f".to_owned(),
      mode: 0o644,
    },
  );
  let ReplyBody::Submitted { version, conflicts } = client.call(&RequestBody::Submit {
    work: meta,
    evidence: Vec::new(),
  }) else {
    panic!("submit meta");
  };
  assert!(
    conflicts.is_empty(),
    "the directory, symlink and mode merge in one increment: {conflicts:?}"
  );
  assert_eq!(version, Some(2), "the metadata increment is version 2");
}

/// A work sets an extended attribute and it commits; a concurrent work setting a *different* value
/// conflicts (§4.16 xattr merge, meta/meta). This is the non-vacuity for the post-state seal: the
/// value round-trips through the post-state, so the green stores the real bytes and the verdict
/// compares them — were the value left zero, both works would seal the same zeros, hash to the same
/// increment identity, and the second would deduplicate to the first's accept instead of conflicting.
/// (The identical-value *accept* path is the engine's `xattr_merges`; over the wire it is exactly
/// this deduplication, so it is not re-asserted here.)
fn declare_xattr_part(client: &mut Client, green: slates_ipc::protocol::VolumeId) {
  use slates_ipc::protocol::WorkOp;
  let set = |client: &mut Client, name: &str, value: &[u8]| -> slates_ipc::protocol::VolumeId {
    let work = work_over(client, green, name);
    declare(
      client,
      work,
      WorkOp::SetXattr {
        path: "f".to_owned(),
        name: "user.k".to_owned(),
        value: value.to_vec(),
      },
    );
    work
  };
  // Both are created and declared while the head is 2, so each is based on 2 and the second meets the
  // first's committed change rather than sitting past it.
  let first = set(client, "xattr-a", b"AA");
  let other = set(client, "xattr-c", b"BB");

  // The first sets the attribute; it advances the green to version 3, storing "AA".
  let ReplyBody::Submitted { version, .. } = client.call(&RequestBody::Submit {
    work: first,
    evidence: Vec::new(),
  }) else {
    panic!("submit xattr-a");
  };
  assert_eq!(version, Some(3), "the first xattr set is version 3");

  // The second, based on 2, sets a different value — it conflicts against the stored "AA".
  let ReplyBody::Submitted { version, conflicts } = client.call(&RequestBody::Submit {
    work: other,
    evidence: Vec::new(),
  }) else {
    panic!("submit xattr-c");
  };
  assert_eq!(
    version, None,
    "a differing concurrent xattr does not accept"
  );
  assert!(
    conflicts.iter().any(|w| w.path == "f"),
    "the conflict names the file: {conflicts:?}"
  );
}

/// The namespace and metadata declarations (§4.16) over the wire: a work declares directories,
/// symlinks, modes and extended attributes — the dimensions beyond content — and they merge. The
/// xattr sub-test is the non-vacuity for the post-state seal (an identical value must accept).
fn merge_declare_scenario() {
  let (daemon, instance) = daemon("merge-declare");
  let mut client = Client::connect(&instance);
  let ReplyBody::GreenCreated { id: green } = client.call(&RequestBody::CreateGreen {
    name: "g6".to_owned(),
    require_evidence: false,
    base: None,
  }) else {
    panic!("create green");
  };
  // Seed a file the metadata operations attach to.
  let seed = work_over(&mut client, green, "seed");
  edit_f(&mut client, seed, 0, 0, b"hello");
  assert!(matches!(
    client.call(&RequestBody::Submit {
      work: seed,
      evidence: Vec::new()
    }),
    ReplyBody::Submitted {
      version: Some(1),
      ..
    }
  ));
  declare_metadata_part(&mut client, green);
  declare_xattr_part(&mut client, green);
  daemon.stop();
}

/// Shape: the records room the metadata-ledger daemon gets above its slabs: 64 KiB — enough for a
/// few 1 MiB volumes (each reserves its 1 % journal budget, its object and a page of snapshot
/// slots, about 15 KiB), so the ledger binds within a handful of creates.
const RECORDS_ROOM: u64 = 64 * 1024;
/// Shape: the most creates the metadata scenario tries before calling the ledger unbounded.
const RECORD_PROBES: usize = 16;

/// AC-0.10 (§4.2 metadata dimension), through the real create and destroy verbs (no mount): a
/// volume's records — its journal budget, its object, its snapshot slab's first segment — are
/// reserved against the shard's metadata ledger at create, so a daemon whose metadata class holds
/// its slabs plus a few volumes' records refuses the next create (`BudgetExceeded`) while bytes and
/// version slots plainly remain, and admits again once a destroy returns the records. Non-vacuous:
/// uncharged, every create here lands (the byte budget and the version slab are far larger).
#[test]
fn a_metadata_class_bounds_the_volume_records_a_shard_admits() {
  let profile = profile();
  let instance = format!("srv-metadata-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance).with_shards(1);
  // The class holds the slabs' maximum footprint plus a few volumes' records, no more.
  config.store.metadata_class_bytes = slab_footprint_of(&config) + RECORDS_ROOM;
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-metadata-{}", std::process::id()),
    },
  )
  .unwrap();
  let mut client = Client::connect(&instance);
  let created = create_until_the_ledger_refuses(&mut client);
  assert!(
    !created.is_empty(),
    "at least one volume's records fit the room"
  );
  // The refusal is the metadata ledger's: bytes and version slots are plainly roomy, the ledger is
  // committed up to its capacity.
  let ReplyBody::DaemonStatus { report } = client.call(&RequestBody::DaemonStatus) else {
    panic!("daemon status");
  };
  let shard = &report.shards[0];
  assert!(
    shard.committed_bytes < shard.reserve_bytes / 2
      && shard.committed_versions < shard.version_slots / 2,
    "bytes ({} of {}) and version slots ({} of {}) plainly remain",
    shard.committed_bytes,
    shard.reserve_bytes,
    shard.committed_versions,
    shard.version_slots
  );
  assert!(
    shard.committed_metadata > 0 && shard.committed_metadata <= shard.metadata_bytes,
    "the records are reserved against the ledger ({} of {})",
    shard.committed_metadata,
    shard.metadata_bytes
  );
  // Destroy one; its records return as the destroy completes, and a fresh volume is admitted again.
  assert!(matches!(
    client.call(&RequestBody::Destroy { volume: created[0] }),
    ReplyBody::Destroyed
  ));
  wait_for_volume_count(&mut client, created.len() - 1);
  assert!(
    matches!(
      client.call(&scratch("records-again")),
      ReplyBody::Created { .. }
    ),
    "the records a destroy returned back a new volume"
  );
  daemon.stop();
}

/// The slabs' maximum footprint under a derived config's caps (§4.2 metadata dimension), computed
/// from a store built on those caps over a one-page arena — arithmetic on the bounds, no segments.
fn slab_footprint_of(config: &DaemonConfig) -> u64 {
  let page = config.page;
  let mut arena = slates_mem::arena::ChunkArena::new(page);
  arena
    .add_region(slates_mem::region::Region::map(page, page, false).unwrap())
    .unwrap();
  slates_vfs::volume::Store::new(
    &slates_vfs::volume::StoreConfig {
      page,
      cache_line: config.cache_line,
      max_dirs: config.store.max_dirs,
      max_inodes: config.store.max_inodes,
      max_chunks: config.store.max_chunks,
      max_dir_blocks: config.store.max_dir_blocks,
      dir_cutover: config.store.dir_cutover,
    },
    arena,
    0,
  )
  .slab_footprint_bytes()
}

/// Creates 1 MiB scratch volumes until the create is refused at the ledger (`BudgetExceeded`),
/// returning the ids that landed; any other outcome, or more than `RECORD_PROBES` creates, fails.
fn create_until_the_ledger_refuses(client: &mut Client) -> Vec<slates_ipc::protocol::VolumeId> {
  let mut created = Vec::new();
  loop {
    assert!(
      created.len() < RECORD_PROBES,
      "the ledger must bind within the records room"
    );
    match client.call(&scratch(&format!("records-{}", created.len()))) {
      ReplyBody::Created { id } => created.push(id),
      ReplyBody::Refused {
        refusal: Refusal::BudgetExceeded { .. },
      } => return created,
      other => panic!("a create lands or refuses at the metadata ledger: {other:?}"),
    }
  }
}

/// Waits, within the credit budget, until the daemon lists exactly `count` volumes (a destroy
/// completes in cooperative slices).
fn wait_for_volume_count(client: &mut Client, count: usize) {
  let started = Instant::now();
  loop {
    let ReplyBody::Listed { volumes } = client.call(&RequestBody::List) else {
      panic!("list");
    };
    if volumes.len() == count {
      return;
    }
    assert!(
      started.elapsed() < CREDIT_WAIT,
      "destroy completes in slices: {volumes:?}"
    );
  }
}

// ---------------------------------------------------------------------------------------------
// The merge *service* (§4.16, D-27; the A-9 integration requirement; AC-6.8, AC-6.13/T-6.15): the
// roles enforced at every verb, a green's chain started only from scratch or a complete immutable
// base, version-pinned attachments moved only by `advance`, and the submission barrier with every
// input retained. Folded into one serial test for the same reason as the lifecycle umbrella.

use slates_ipc::protocol::{GreenBase, ReadAt};

/// Creates a green from scratch.
fn green(
  client: &mut Client,
  name: &str,
  require_evidence: bool,
) -> slates_ipc::protocol::VolumeId {
  let ReplyBody::GreenCreated { id } = client.call(&RequestBody::CreateGreen {
    name: name.to_owned(),
    require_evidence,
    base: None,
  }) else {
    panic!("create green {name}");
  };
  id
}

/// Reads `path` of `volume` at `at`: the bytes, or the refusal.
fn read(
  client: &mut Client,
  volume: slates_ipc::protocol::VolumeId,
  path: &str,
  at: ReadAt,
) -> Result<Vec<u8>, Refusal> {
  match client.call(&RequestBody::Read {
    volume,
    path: path.to_owned(),
    at,
  }) {
    ReplyBody::ReadBytes { bytes } => Ok(bytes),
    ReplyBody::Refused { refusal } => Err(refusal),
    other => panic!("read: {other:?}"),
  }
}

/// The refusal a request gets, or a panic naming the reply that was not one.
fn refusal_of(client: &mut Client, body: &RequestBody) -> Refusal {
  match client.call(body) {
    ReplyBody::Refused { refusal } => refusal,
    other => panic!("{body:?} was not refused: {other:?}"),
  }
}

/// Declares a splice on `work`'s `path` (any path, unlike [`edit_f`]).
fn edit_at(
  client: &mut Client,
  work: slates_ipc::protocol::VolumeId,
  path: &str,
  at: u64,
  delete_len: u64,
  bytes: &[u8],
) {
  let reply = client.call(&RequestBody::Edit {
    work,
    path: path.to_owned(),
    at,
    delete_len,
    bytes: bytes.to_vec(),
  });
  assert!(matches!(reply, ReplyBody::Edited), "edit {path}: {reply:?}");
}

/// Submits `work` with no evidence and asserts it was accepted as `version`.
fn submit_accepted(client: &mut Client, work: slates_ipc::protocol::VolumeId, version: u64) {
  let reply = client.call(&RequestBody::Submit {
    work,
    evidence: Vec::new(),
  });
  match reply {
    ReplyBody::Submitted {
      version: Some(got),
      conflicts,
    } if got == version && conflicts.is_empty() => {}
    other => panic!("submit expected version {version}: {other:?}"),
  }
}

/// A `Submit` request with no evidence.
fn submit_of(work: slates_ipc::protocol::VolumeId) -> RequestBody {
  RequestBody::Submit {
    work,
    evidence: Vec::new(),
  }
}

/// An `Edit` request creating `f` on `work`.
fn edit_on(work: slates_ipc::protocol::VolumeId) -> RequestBody {
  RequestBody::Edit {
    work,
    path: "f".to_owned(),
    at: 0,
    delete_len: 0,
    bytes: b"x".to_vec(),
  }
}

/// Destroys `volume`, asserting the reply.
fn destroy_ok(client: &mut Client, volume: slates_ipc::protocol::VolumeId) {
  let reply = client.call(&RequestBody::Destroy { volume });
  assert!(matches!(reply, ReplyBody::Destroyed), "destroy: {reply:?}");
}

/// Snapshots `volume`; the snapshot id.
fn snapshot_of(
  client: &mut Client,
  volume: slates_ipc::protocol::VolumeId,
) -> slates_ipc::protocol::SnapshotId {
  let ReplyBody::Snapshotted { id } = client.call(&RequestBody::Snapshot { volume }) else {
    panic!("snapshot");
  };
  id
}

/// Attaches a reader to `green`; the attachment id and the version it pins.
fn attach_reader(client: &mut Client, green: slates_ipc::protocol::VolumeId) -> (u64, Option<u64>) {
  let ReplyBody::Attached {
    attachment,
    version,
    lease_epoch,
    ..
  } = client.call(&RequestBody::Attach {
    volume: green,
    snapshot: None,
    intent: Intent::Read,
    form: slates_ipc::protocol::AttachRequest::Root,
  })
  else {
    panic!("attach");
  };
  assert_eq!(lease_epoch, None, "a reader takes no lease");
  (attachment, version)
}

/// Advances `attachment` to `version` (or the head); the version pinned and the paths invalidated.
fn advance(client: &mut Client, attachment: u64, version: Option<u64>) -> (u64, Vec<String>) {
  let ReplyBody::Advanced {
    version,
    invalidated,
  } = client.call(&RequestBody::Advance {
    attachment,
    version,
  })
  else {
    panic!("advance");
  };
  (version, invalidated)
}

/// A green is written by nothing but its merge task: an edit, a declaration, a write attachment, a
/// snapshot and a resize of it refuse `ReadOnlyVolume`.
fn role_green_is_read_only_part(client: &mut Client, g: slates_ipc::protocol::VolumeId) {
  assert_eq!(refusal_of(client, &edit_on(g)), Refusal::ReadOnlyVolume);
  let declare_on_green = RequestBody::Declare {
    work: g,
    op: slates_ipc::protocol::WorkOp::Mkdir {
      path: "d".to_owned(),
    },
  };
  assert_eq!(
    refusal_of(client, &declare_on_green),
    Refusal::ReadOnlyVolume
  );
  let write_attach = RequestBody::Attach {
    volume: g,
    snapshot: None,
    intent: Intent::Write,
    form: slates_ipc::protocol::AttachRequest::Root,
  };
  assert_eq!(refusal_of(client, &write_attach), Refusal::ReadOnlyVolume);
  assert_eq!(
    refusal_of(client, &RequestBody::Snapshot { volume: g }),
    Refusal::ReadOnlyVolume
  );
  let resize = RequestBody::Resize {
    volume: g,
    size: SizeClass::Dynamic { max: 1 << 20 },
  };
  assert_eq!(refusal_of(client, &resize), Refusal::ReadOnlyVolume);
}

/// A work verb on what is not a work is `NotWork`; a green verb on what is not a green is `NotGreen`.
fn role_kind_mismatch_part(
  client: &mut Client,
  g: slates_ipc::protocol::VolumeId,
  w: slates_ipc::protocol::VolumeId,
  p: slates_ipc::protocol::VolumeId,
) {
  assert_eq!(refusal_of(client, &edit_on(p)), Refusal::NotWork);
  assert_eq!(refusal_of(client, &submit_of(g)), Refusal::NotWork);
  assert_eq!(
    refusal_of(client, &RequestBody::Rebase { work: p }),
    Refusal::NotWork
  );
  assert_eq!(
    refusal_of(client, &RequestBody::Versions { green: w }),
    Refusal::NotGreen
  );
  let changed_since_plain = RequestBody::ChangedSince {
    green: p,
    version: 0,
  };
  assert_eq!(refusal_of(client, &changed_since_plain), Refusal::NotGreen);
  let work_over_plain = RequestBody::CreateWork {
    green: p,
    name: "over-plain".to_owned(),
  };
  assert_eq!(refusal_of(client, &work_over_plain), Refusal::NotGreen);
}

/// A store-backed verb a merge volume cannot serve is refused `Unsupported`, naming the verb.
fn role_store_verbs_part(
  client: &mut Client,
  g: slates_ipc::protocol::VolumeId,
  w: slates_ipc::protocol::VolumeId,
) {
  let clone = RequestBody::Clone {
    volume: g,
    snapshot: slates_ipc::protocol::SnapshotId::default(),
    name: "c".to_owned(),
  };
  let Refusal::Unsupported { feature } = refusal_of(client, &clone) else {
    panic!("clone of a green");
  };
  assert!(feature.contains("clone"), "{feature}");
  let land = RequestBody::Land {
    volume: g,
    snapshot: None,
    target: "/nonexistent".to_owned(),
    filter: Filter::default(),
    grant: None,
  };
  let Refusal::Unsupported { feature } = refusal_of(client, &land) else {
    panic!("land of a green");
  };
  assert!(feature.contains("landing"), "{feature}");
  let Refusal::Unsupported { feature } = refusal_of(client, &RequestBody::Snapshot { volume: w })
  else {
    panic!("snapshot of a work");
  };
  assert!(feature.contains("work"), "{feature}");
}

/// A merge volume reports status; a destroyed work is gone; a destroyed green makes its remaining
/// work's submit `UnknownBase`.
fn role_destroy_part(
  client: &mut Client,
  g: slates_ipc::protocol::VolumeId,
  w: slates_ipc::protocol::VolumeId,
) {
  let ReplyBody::Status { report } = client.call(&RequestBody::Status { volume: g }) else {
    panic!("status of a green");
  };
  assert_eq!(report.head.value, 0, "a fresh green's head version");
  destroy_ok(client, w);
  assert_eq!(refusal_of(client, &edit_on(w)), Refusal::NotFound);
  let orphan = work_over(client, g, "orphan");
  edit_f(client, orphan, 0, 0, b"hello");
  destroy_ok(client, g);
  assert_eq!(
    refusal_of(client, &submit_of(orphan)),
    Refusal::UnknownBase {
      green: g,
      version: 0
    }
  );
}

/// Evidence: required by the green, refused when absent, accepted when carried.
fn role_evidence_part(client: &mut Client) {
  let strict = green(client, "strict", true);
  let sw = work_over(client, strict, "sw");
  edit_f(client, sw, 0, 0, b"evidenced");
  assert_eq!(
    refusal_of(client, &submit_of(sw)),
    Refusal::EvidenceRequired
  );
  let reply = client.call(&RequestBody::Submit {
    work: sw,
    evidence: vec![[7u8; 32]],
  });
  assert!(
    matches!(
      reply,
      ReplyBody::Submitted {
        version: Some(1),
        ..
      }
    ),
    "{reply:?}"
  );
}

/// AC-6.8 ("Green is written by nothing but the merge task: … SDK writes refuse"), §4.4's merge
/// refusals: every client mutation of a green refuses `ReadOnlyVolume`; a verb that needs a work
/// refuses `NotWork` on a plain volume; a verb that needs a green refuses `NotGreen` on a work or a
/// plain volume; a store-backed verb a merge volume cannot serve refuses `Unsupported` naming it; a
/// destroyed green makes its works' submits `UnknownBase`; a green that requires evidence refuses
/// `EvidenceRequired`. Non-vacuous: every one of these answered `NotFound` before the roles were
/// enforced at the service.
fn merge_role_scenario() {
  let (daemon, instance) = daemon("merge-roles");
  let mut client = Client::connect(&instance);
  let g = green(&mut client, "g", false);
  let w = work_over(&mut client, g, "w");
  let ReplyBody::Created { id: p } = client.call(&scratch("p")) else {
    panic!("create plain");
  };
  role_green_is_read_only_part(&mut client, g);
  role_kind_mismatch_part(&mut client, g, w, p);
  role_store_verbs_part(&mut client, g, w);
  role_destroy_part(&mut client, g, w);
  role_evidence_part(&mut client);
  daemon.stop();
}

/// Writes `text` into `path` on the host through the shell (a test's own disk write, outside slates).
fn host_write(path: &str, text: &str) {
  let wrote = std::process::Command::new("sh")
    .arg("-c")
    .arg(format!("printf '%s' '{text}' > '{path}'"))
    .output()
    .unwrap();
  assert!(wrote.status.success(), "host write of {path}");
}

/// Creates an overlay volume over `dir`.
fn overlay_over(client: &mut Client, name: &str, dir: &str) -> slates_ipc::protocol::VolumeId {
  let ReplyBody::Created { id } = client.call(&RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Dynamic { max: 1 << 24 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: Some(dir.to_owned()),
  }) else {
    panic!("create overlay");
  };
  id
}

/// A green over the snapshot: the request.
fn green_over(
  name: &str,
  volume: slates_ipc::protocol::VolumeId,
  snapshot: slates_ipc::protocol::SnapshotId,
) -> RequestBody {
  RequestBody::CreateGreen {
    name: name.to_owned(),
    require_evidence: false,
    base: Some(GreenBase { volume, snapshot }),
  }
}

/// A snapshot still served live is refused as a base; the whole base pinned and snapshotted again
/// is accepted, and the green's version 0 reads the pinned bytes.
fn base_completeness_part(
  client: &mut Client,
  overlay: slates_ipc::protocol::VolumeId,
) -> slates_ipc::protocol::VolumeId {
  let live = snapshot_of(client, overlay);
  assert_eq!(
    refusal_of(client, &green_over("g-live", overlay, live)),
    Refusal::ConsistentBaseUnavailable,
    "a snapshot still served from the host directory is no immutable base"
  );
  let pinned = client.call(&RequestBody::Pin {
    volume: overlay,
    paths: None,
  });
  assert!(
    matches!(pinned, ReplyBody::Pinned { entries: 1 }),
    "{pinned:?}"
  );
  let complete = snapshot_of(client, overlay);
  let ReplyBody::GreenCreated { id: g } = client.call(&green_over("g-base", overlay, complete))
  else {
    panic!("create green over a complete base");
  };
  assert_eq!(green_head(client, g), 0, "the origin is version 0");
  assert_eq!(
    read(client, g, "f.txt", ReadAt::Version { version: 0 }).unwrap(),
    b"disk bytes"
  );
  g
}

/// The host moves on; no green version does, nor a work cloned from the green, and a merge on top
/// of the origin leaves version 0 as it was.
fn base_immutable_part(client: &mut Client, g: slates_ipc::protocol::VolumeId, dir: &str) {
  host_write(&format!("{dir}/f.txt"), "HOST MOVED");
  assert_eq!(
    read(client, g, "f.txt", ReadAt::Head).unwrap(),
    b"disk bytes",
    "the green's head is the origin's bytes, not the disk's"
  );
  let w = work_over(client, g, "w");
  assert_eq!(
    read(client, w, "f.txt", ReadAt::Head).unwrap(),
    b"disk bytes",
    "a work clones the green version, never the disk"
  );
  edit_at(client, w, "f.txt", 0, 0, b"agent: ");
  submit_accepted(client, w, 1);
  assert_eq!(
    read(client, g, "f.txt", ReadAt::Version { version: 0 }).unwrap(),
    b"disk bytes",
    "version 0 is immutable"
  );
  assert_eq!(
    read(client, g, "f.txt", ReadAt::Version { version: 1 }).unwrap(),
    b"agent: disk bytes"
  );
  assert_eq!(green_head(client, g), 1);
}

/// The A-9 integration requirement ("Green's immutable version chain starts from scratch or a
/// complete immutable base, never an implicitly live host directory"; AC-6.13): a green over a
/// snapshot of an overlay that is still served live is refused `ConsistentBaseUnavailable`; once
/// the whole base is pinned and snapshotted the green is created with that snapshot as version 0;
/// an edit of the host directory afterwards changes no green version, nor a work cloned from it.
/// Non-vacuous: the host file is rewritten with different bytes of the same length, so a version
/// that read the disk would show the new bytes.
fn green_over_base_scenario() {
  let (daemon, instance) = daemon("merge-base");
  let mut client = Client::connect(&instance);
  let dir = target_dir();
  host_write(&format!("{}/f.txt", dir.path), "disk bytes");
  let overlay = overlay_over(&mut client, "over", &dir.path);
  let g = base_completeness_part(&mut client, overlay);
  base_immutable_part(&mut client, g, &dir.path);
  daemon.stop();
}

/// A reader pins the head at attach time and does not move when a merge lands a new head.
fn attachment_pin_part(client: &mut Client, g: slates_ipc::protocol::VolumeId) -> u64 {
  let a = work_over(client, g, "a");
  edit_f(client, a, 0, 0, b"first");
  submit_accepted(client, a, 1);
  let (attachment, version) = attach_reader(client, g);
  assert_eq!(
    version,
    Some(1),
    "the attachment pins the head at attach time"
  );
  // A second agent lands version 2 (`f` grown, a new file `g`).
  let b = work_over(client, g, "b");
  edit_f(client, b, 5, 0, b"+second");
  edit_at(client, b, "g", 0, 0, b"new");
  submit_accepted(client, b, 2);
  let pinned = ReadAt::Attachment { attachment };
  assert_eq!(
    read(client, g, "f", pinned).unwrap(),
    b"first",
    "the attached view did not move with the head"
  );
  assert_eq!(
    read(client, g, "g", pinned),
    Err(Refusal::NotFound),
    "a file born after the pin is not in the attached view"
  );
  assert_eq!(read(client, g, "f", ReadAt::Head).unwrap(), b"first+second");
  attachment
}

/// `advance` re-pins and names exactly the paths the span changed, forward and back; a version past
/// the head is `UnknownBase`; a detached attachment reads nothing.
fn attachment_advance_part(
  client: &mut Client,
  g: slates_ipc::protocol::VolumeId,
  attachment: u64,
) {
  let pinned = ReadAt::Attachment { attachment };
  let (version, invalidated) = advance(client, attachment, None);
  assert_eq!(version, 2);
  assert_eq!(invalidated, vec!["f".to_owned(), "g".to_owned()]);
  assert_eq!(read(client, g, "f", pinned).unwrap(), b"first+second");
  assert_eq!(read(client, g, "g", pinned).unwrap(), b"new");
  let (version, invalidated) = advance(client, attachment, Some(1));
  assert_eq!(
    (version, invalidated.len()),
    (1, 2),
    "back to 1: the same span"
  );
  assert_eq!(read(client, g, "f", pinned).unwrap(), b"first");
}

/// A version past the head is `UnknownBase`; a detached attachment reads nothing.
fn attachment_bounds_part(client: &mut Client, g: slates_ipc::protocol::VolumeId, attachment: u64) {
  let pinned = ReadAt::Attachment { attachment };
  let past = RequestBody::Advance {
    attachment,
    version: Some(9),
  };
  assert_eq!(
    refusal_of(client, &past),
    Refusal::UnknownBase {
      green: g,
      version: 9
    }
  );
  let detached = client.call(&RequestBody::Detach { attachment });
  assert!(matches!(detached, ReplyBody::Detached), "{detached:?}");
  assert_eq!(read(client, g, "f", pinned), Err(Refusal::NotFound));
}

/// §4.16 "Attachments and versions" (AC-6.8: "an attachment's view never changes without
/// `advance`"): a read attachment of a green pins the head at attach time; a merge that lands a new
/// head leaves the attached view unchanged; `advance` re-pins and names exactly the paths the span
/// changed; a version past the head is `UnknownBase`; a detached attachment reads nothing.
fn green_attachment_scenario() {
  let (daemon, instance) = daemon("merge-attach");
  let mut client = Client::connect(&instance);
  let g = green(&mut client, "g", false);
  let attachment = attachment_pin_part(&mut client, g);
  attachment_advance_part(&mut client, g, attachment);
  attachment_bounds_part(&mut client, g, attachment);
  daemon.stop();
}

/// A submit seals exactly the operations declared before it; a later edit is the next increment, and
/// the accepted work equals the green at its new base.
fn barrier_part(
  client: &mut Client,
  g: slates_ipc::protocol::VolumeId,
) -> slates_ipc::protocol::VolumeId {
  let w = work_over(client, g, "w");
  edit_f(client, w, 0, 0, b"hello");
  submit_accepted(client, w, 1);
  edit_f(client, w, 5, 0, b" world");
  assert_eq!(
    read(client, g, "f", ReadAt::Version { version: 1 }).unwrap(),
    b"hello",
    "the sealed version holds only what was declared before the seal"
  );
  let ReplyBody::Submitted { version, conflicts } = client.call(&submit_of(w)) else {
    panic!("second submit");
  };
  assert!(
    conflicts.is_empty(),
    "the later edit is a new increment, never a conflict with the work's own committed bytes: {conflicts:?}"
  );
  assert_eq!(version, Some(2));
  assert_eq!(
    read(client, g, "f", ReadAt::Version { version: 2 }).unwrap(),
    b"hello world"
  );
  assert_eq!(
    read(client, w, "f", ReadAt::Head).unwrap(),
    b"hello world",
    "the accepted work equals the green at its new base"
  );
  w
}

/// Another agent lands a disjoint file after the re-based work; destroying the work that produced
/// versions 1 and 2 frees none of their inputs.
fn retention_part(
  client: &mut Client,
  g: slates_ipc::protocol::VolumeId,
  w: slates_ipc::protocol::VolumeId,
) {
  let other = work_over(client, g, "other");
  edit_at(client, other, "h", 0, 0, b"other");
  submit_accepted(client, other, 3);
  destroy_ok(client, w);
  assert_eq!(
    read(client, g, "f", ReadAt::Version { version: 1 }).unwrap(),
    b"hello"
  );
  assert_eq!(
    read(client, g, "f", ReadAt::Version { version: 2 }).unwrap(),
    b"hello world"
  );
  assert_eq!(read(client, g, "f", ReadAt::Head).unwrap(), b"hello world");
}

/// §4.16 "Submission" ("seal the work volume … the contributing attachment barrier") and the
/// retention of every input to the verdict (the A-9 requirement): a submit seals exactly the
/// operations declared before it — an edit after the seal lands in the *next* increment, never the
/// sealed one — so the same work submits again cleanly with only its later edit; the accepted
/// work's content is the green's at the new version; and the inputs of a committed version stay
/// readable after the work that produced them is destroyed. Non-vacuous: before the accepted
/// work's journal was consumed, its second submit re-declared the first edit against the old base
/// and refused a create/create conflict with its own committed bytes
/// (`docs/bugs/2026-09-13-work-resubmit-self-conflict.md`).
fn submission_barrier_scenario() {
  let (daemon, instance) = daemon("merge-barrier");
  let mut client = Client::connect(&instance);
  let g = green(&mut client, "g", false);
  let w = barrier_part(&mut client, g);
  retention_part(&mut client, g, w);
  daemon.stop();
}

/// AC-6.8, AC-6.13/T-6.15 (§4.16 as a service): the roles at every verb, a green only from scratch
/// or a complete immutable base, version-pinned attachments moved only by `advance`, and the
/// submission barrier with every input retained — one daemon at a time, as the lifecycle umbrella.
#[test]
fn the_merge_service_enforces_roles_pins_versions_and_seals_behind_the_barrier() {
  merge_role_scenario();
  green_over_base_scenario();
  green_attachment_scenario();
  submission_barrier_scenario();
}
