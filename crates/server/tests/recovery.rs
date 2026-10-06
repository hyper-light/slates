//! The recovery oracle (§2.6 boot step 2, §4.8 "Recovery" and the A-9 invariants, D-18; AC-2.12 /
//! T-2.14; GAP-A9-6, BUG-11): a daemon restart preserves acknowledged **content** and its atomic
//! completion, not only catalog identity.
//!
//! The test plays the anchor (the client restart test's pattern): it holds one anchor segment and its
//! content object across two in-process daemons. Content is written through the daemon's own NFS
//! transport — the hand-rolled ONC RPC client of `common::nfs`, so the bytes travel the same path a
//! kernel `mount_nfs` sends them down (client → NFS → the shard's real volume) with no mount and no
//! privilege — and the snapshot, clone, status and retry go through the typed client verbs. After the
//! restart, every byte the first daemon acknowledged must read back identically through the same
//! verbs, or the daemon must refuse explicitly; an empty or older file is the acknowledged data loss
//! the design forbids ("rebuilding a scratch volume from only a quota and id loses acknowledged
//! data", §4.8).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// These integration tests drive the daemon's NFS-loopback transport, the fleet's TCP
// transport and rustix syscalls — all macOS/Linux; on Windows the daemon mounts through WinFsp and
// the fleet transport is QUIC-over-UDP, so these particular tests are unix (as `virtiofs.rs` is).
#![cfg(unix)]

use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use slates_anchor::{AnchorSegment, RegionKind};
use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, NamePolicy, SizeClass, SnapshotId, VolumeId,
};
use slates_ipc::protocol::{Filter, ReplyBody, RequestBody};
use slates_machine::MachineProfile;
use slates_server::{BootFault, BootFaultKind, Daemon, DaemonConfig};
use slates_wire::request::RequestId;

mod common;
use common::anchor::{anchor_segment, source_of};
use common::nfs::{create, fsstat, lookup, mount, read, write};

/// Shape: shards per test daemon: two, so the volume can live on a shard other than the control
/// shard that accepts the NFS connection, and the cross-shard bridge route is on the recovery path.
const TEST_SHARDS: u16 = 2;
/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);
/// Shape: the bytes written before the snapshot — what the snapshot freezes.
const BEFORE: &[u8] = b"the bytes the snapshot froze: alpha bravo charlie delta echo foxtrot\n";
/// Shape: the bytes written over the same file after the snapshot — the diverged head. Longer than
/// [`BEFORE`] and written at offset zero, so the head's file is exactly these bytes.
const AFTER: &[u8] =
  b"the bytes the head holds after the snapshot: golf hotel india juliet kilo lima mike november\n";

/// The product's own deadlines (`Deadlines::derive` over the anchor's liveness budget and the recovery
/// budget), never a shorter hand-picked reply clock that calls a live daemon stalled
/// (`docs/bugs/2026-09-28-the-client-tests-judged-a-live-daemon-by-a-shorter-clock.md`).
fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

fn connect(instance: &str) -> Client {
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

fn scratch(name: &str) -> CreateSpec {
  CreateSpec {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// AC-2.3 (§4.8 transactions; AUD-06): a verb whose record cannot be made durable is refused **typed**
/// (`Unpublished`), its effects and its completion record are rolled back together, a retry under the
/// same id **re-executes** (it is never answered a success from memory — there is none), and a restart
/// over the same segment agrees with the live daemon: the volume the retry created is there, once, and
/// the retried id still meets its completion record. Non-vacuous: the shard's rollback counter moved,
/// and the refusal is the typed one. The failure is injected at the database's publication (the
/// segment refusing the record) so the whole recorded-verb path above it — dispatch, the completion
/// record, the commit, the reply — runs exactly as it would on a segment that cannot publish.
#[test]
fn an_unpublished_verb_is_refused_typed_a_retry_re_executes_and_a_restart_agrees() {
  let profile = common::machine_profile();
  let instance = format!("srv-unpublished-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("unpublished", &profile, &config);

  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let durable = client.create(&scratch("durable")).unwrap();

  let create_id = refused_unpublished_create(&first, &mut client);

  // The retry under the same id re-executes — the volume is created now, durably — rather than
  // reading a success that was never published.
  let retried = retry_create(&mut client, create_id, "unpublished");
  let ReplyBody::Created { id: created } = retried else {
    panic!("the retry re-executes the create: {retried:?}");
  };
  assert_ne!(created, durable, "a distinct volume");
  assert_eq!(
    names_listed(&mut client),
    vec!["durable".to_owned(), "unpublished".to_owned()]
  );

  // The restart over the same segment agrees: the effect and its completion survived together.
  let member = first.member_identity().unwrap();
  first.stop();
  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  assert_eq!(second.member_identity(), Ok(member), "a warm restart");
  assert_eq!(
    names_listed(&mut client),
    vec!["durable".to_owned(), "unpublished".to_owned()],
    "the retried create is durable: present once after the restart"
  );
  assert_eq!(
    retry_create(&mut client, create_id, "unpublished"),
    ReplyBody::Created { id: created },
    "after the restart the id meets the completion record the retry published — the same volume, no \
     second create"
  );
  assert_eq!(
    rollbacks_of(&second),
    0,
    "the restarted daemon rolled nothing back"
  );
  second.stop();
}

/// AC-2.3 (AUD-06's contract, as the CI Linux lane broke it): a verb refused `Unpublished` stays
/// retryable **across the client's acknowledgements**. The client acknowledges its received replies
/// every `slots / 2` requests (§4.9, so the daemon's retained records stay bounded); on a runner whose
/// ring is small that acknowledgement fell between the refusal and the retry, took the refused
/// sequence with it, and the retry was answered `DuplicateRequest` — the contract's "retry under the
/// same id" lost to the bookkeeping. Here the acknowledgement is forced right after the refusal: the
/// retry must still re-execute, so the client's watermark must stop below an unpublished id until it
/// is retried. Non-vacuous: before the fix the retry was refused `DuplicateRequest`.
#[test]
fn an_unpublished_verb_stays_retryable_across_the_clients_acknowledgement() {
  let profile = common::machine_profile();
  let instance = format!("srv-unpublished-ack-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("unpublished-ack", &profile, &config);

  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let durable = client.create(&scratch("durable")).unwrap();
  let create_id = refused_unpublished_create(&daemon, &mut client);

  // The client acknowledges everything it has received — the refusal included, were it naive.
  client
    .acknowledge_all()
    .expect("the acknowledgement is served");
  assert!(
    client.acknowledged() < create_id.sequence,
    "the watermark stops below the unpublished id ({} < {})",
    client.acknowledged(),
    create_id.sequence
  );

  let retried = retry_create(&mut client, create_id, "unpublished");
  let ReplyBody::Created { id: created } = retried else {
    panic!("the retry re-executes the create after the acknowledgement: {retried:?}");
  };
  assert_ne!(created, durable, "a distinct volume");
  // Retried and answered, the id no longer holds the watermark back.
  client
    .acknowledge_all()
    .expect("the acknowledgement is served");
  assert!(
    client.acknowledged() >= create_id.sequence,
    "once retried the id is acknowledged like any other"
  );
  assert_eq!(
    names_listed(&mut client),
    vec!["durable".to_owned(), "unpublished".to_owned()]
  );
  daemon.stop();
  drop(segment);
}

/// The refused phase: the next publication on whichever shard runs the create is refused before its
/// record is appended, so the create of "unpublished" comes back typed `Unpublished`, the shard
/// counts one rollback, and nothing of the verb is listed. Returns the refused request's id, for the
/// retry. Clears the faults still pending on the other shards.
fn refused_unpublished_create(daemon: &Daemon, client: &mut Client) -> RequestId {
  daemon
    .inject_publication_fault(Some(slates_db::replay::PublicationFault::BeforeAppend))
    .expect("the fault is installed");
  let refused = client.create(&scratch("unpublished"));
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(
        slates_ipc::protocol::Refusal::Unpublished { .. }
      ))
    ),
    "the verb is refused typed as unpublished, not served: {refused:?}"
  );
  let create_id = client.last_request();
  assert_eq!(
    rollbacks_of(daemon),
    1,
    "the shard rolled the transaction back (counted)"
  );
  daemon
    .inject_publication_fault(None)
    .expect("the faults still pending on the other shards are cleared");
  assert_eq!(
    names_listed(client),
    vec!["durable".to_owned()],
    "nothing of the refused verb is visible: the rolled-back volume is not listed"
  );
  create_id
}

/// The volume names the daemon lists, sorted.
fn names_listed(client: &mut Client) -> Vec<String> {
  let mut names: Vec<String> = client
    .list()
    .unwrap()
    .iter()
    .map(|v| v.name.clone())
    .collect();
  names.sort();
  names
}

/// Retries the scratch create of `name` under `id` — the daemon answers from its completion record
/// when it served the id, else serves it now.
fn retry_create(client: &mut Client, id: RequestId, name: &str) -> ReplyBody {
  client
    .retry(
      id,
      &RequestBody::Create {
        name: name.to_owned(),
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    )
    .unwrap()
}

/// Transactions rolled back across every shard of `daemon` (`Daemon::db_publication_counters`).
fn rollbacks_of(daemon: &Daemon) -> u64 {
  daemon
    .db_publication_counters()
    .unwrap()
    .iter()
    .map(|c| c.rollbacks)
    .sum()
}

/// An NFS connection to the daemon's loopback port.
fn nfs_stream(daemon: &Daemon) -> TcpStream {
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  TcpStream::connect(("127.0.0.1", port)).unwrap()
}

/// The NFS mount path carrying an owner mount capability for the volume named `name` on `daemon`
/// (§4.13; AUD-01): a name alone reaches nothing.
fn capability_path(daemon: &Daemon, name: &str) -> String {
  daemon
    .mount_capability(name)
    .expect("the name's owner shard answers")
    .expect("a volume by that name is served there")
}

/// The bytes of `/<volume>/<file>` read through a fresh NFS connection to `daemon`.
fn read_file(daemon: &Daemon, volume: &str, file: &str) -> Vec<u8> {
  let mut stream = nfs_stream(daemon);
  let root = mount(&mut stream, &capability_path(daemon, volume), 1);
  let file = lookup(&mut stream, &root, file, 2);
  read(&mut stream, &file, 3)
}

/// A clone name that routes, by name, to the partition its origin `origin_name` lives on. A clone
/// is created on its origin's partition (it shares the origin's copy-on-write tree, so it must live
/// in the same store), but the NFS host root resolves a name through `owner_of_name` — a hash — so
/// a clone whose name hashes to another partition is unreachable by name over the mount transport
/// (a pre-existing sibling outside the recovery path, reported, not this oracle's subject). The
/// name is chosen here so the oracle reads the snapshot's content through the route that exists.
fn clone_name_on_origin_partition(origin_name: &str, stem: &str) -> String {
  let partitions = usize::from(TEST_SHARDS);
  let origin = slates_server::verbs::owner_of_name(origin_name, partitions);
  (0..partitions)
    .map(|suffix| format!("{stem}-{suffix}"))
    .find(|candidate| slates_server::verbs::owner_of_name(candidate, partitions) == origin)
    .unwrap_or_else(|| stem.to_owned())
}

/// AC-2.12 / T-2.14 (GAP-A9-6): write bytes, snapshot, diverge the head, restart the daemon over the
/// same anchor-owned RAM; expect the head's acknowledged bytes and the snapshot's frozen bytes to read
/// back byte-identical through the same verbs, the snapshot still held and the head still pointing at
/// it. Non-vacuous: the head is written *after* the last control-plane publish (the snapshot), so a
/// recovery that only republishes on control verbs — or that rebuilds scratch content empty (BUG-11) —
/// reads back the frozen bytes or nothing, and fails.
#[test]
fn acknowledged_content_and_its_snapshot_survive_a_daemon_restart_byte_for_byte() {
  let profile = common::machine_profile();
  let instance = format!("srv-recover-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("recover", &profile, &config);

  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let kept = client.create(&scratch("kept")).unwrap();
  {
    let mut stream = nfs_stream(&first);
    let root = mount(&mut stream, &capability_path(&first, "kept"), 1);
    let file = create(&mut stream, &root, "f", 2);
    write(&mut stream, &file, BEFORE, 3);
    let snapshot = client.snapshot(kept).unwrap();
    assert_eq!(client.status(kept).unwrap().head.value, snapshot.value);
    // Diverge the head after the snapshot: acknowledged FILE_SYNC by the first daemon.
    write(&mut stream, &file, AFTER, 4);
    assert_eq!(
      read(&mut stream, &file, 5),
      AFTER,
      "the head diverged before the restart"
    );
  }
  let snapshot = client.status(kept).unwrap().head;
  let member = first.member_identity().unwrap();
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  assert_eq!(
    second.member_identity(),
    Ok(member),
    "retained Raft state preserves the voter; a warm restart needs no bootstrap"
  );
  let report = client.status(kept).unwrap();
  assert_eq!(
    client.reconnects(),
    1,
    "the session reconnected under its id"
  );
  assert_eq!(
    report.snapshots, 1,
    "the snapshot was recovered, not reconciled away"
  );
  assert_eq!(
    report.head.value, snapshot.value,
    "the head still points at the recovered snapshot"
  );
  assert_eq!(
    read_file(&second, "kept", "f"),
    AFTER,
    "the head's acknowledged bytes, written after the snapshot, survived the restart"
  );
  // The snapshot's frozen bytes, read through a clone of it — the only path that serves a snapshot's
  // content over the mount transport.
  let clone_name = clone_name_on_origin_partition("kept", "kept-at-snapshot");
  client.clone_snapshot(kept, snapshot, &clone_name).unwrap();
  assert_eq!(
    read_file(&second, &clone_name, "f"),
    BEFORE,
    "the snapshot's frozen bytes survived the restart"
  );
  second.stop();
  drop(segment);
}

/// A one-file archive of `bytes` as one raw chunk, with the format's own header bounds — the replica the
/// holder tests place.
fn replica_archive(bytes: &[u8]) -> slates_archive::Archive {
  use slates_archive::format::{MAX_BASE_PAGE_BYTES, MAX_CHUNK_BYTES};
  use slates_archive::{Archive, Entry, Extent, Node, NodeMeta};
  let chunk = Archive::raw_chunk(bytes.to_vec());
  Archive {
    base_page_size: u32::try_from(MAX_BASE_PAGE_BYTES).unwrap(),
    chunk_min: u32::try_from(MAX_CHUNK_BYTES).unwrap(),
    chunk_max: u32::try_from(MAX_CHUNK_BYTES).unwrap(),
    created_unix: 0,
    volume_id: 0,
    snapshot_id: 0,
    name_policy_id: 0,
    unicode_version: 0,
    root_meta: NodeMeta::default(),
    manifest: Node::Directory(vec![Entry {
      name: "f".to_owned(),
      meta: NodeMeta {
        size: chunk.raw_len,
        ..NodeMeta::default()
      },
      node: Node::File(vec![Extent {
        offset: 0,
        len: chunk.raw_len,
        chunk: chunk.identity,
        chunk_offset: 0,
      }]),
    }]),
    chunks: vec![chunk],
  }
}

/// Places `archive` for `object` at `sequence` on `daemon` through the holder's production path, as an owner
/// does (AUD-29-55): the offer, then one chunk exchange per chunk the reply names. Returns the last reply —
/// the acknowledgement when the placement completed, else whatever the holder answered.
fn place_replica(
  daemon: &Daemon,
  object: slates_db::register::ObjectId,
  sequence: u64,
  archive: &slates_archive::Archive,
) -> Vec<u8> {
  use slates_cluster::content::{ContentMessage, chunk_requests, offer_request};
  let reply = daemon
    .serve_content_as_authorized(offer_request(archive, object, sequence))
    .unwrap();
  let Ok(ContentMessage::Missing { missing, .. }) = ContentMessage::decode(&reply) else {
    return reply;
  };
  let mut last = reply;
  for request in chunk_requests(archive, object, sequence, &missing) {
    last = daemon.serve_content_as_authorized(request).unwrap();
  }
  last
}

/// An archive of one file per entry of `pieces`, each piece one raw chunk — a placement of as many chunks.
fn pieces_archive(pieces: &[Vec<u8>]) -> slates_archive::Archive {
  use slates_archive::{Archive, Entry, Extent, Node, NodeMeta};
  let chunks: Vec<_> = pieces
    .iter()
    .map(|piece| Archive::raw_chunk(piece.clone()))
    .collect();
  let entries = chunks
    .iter()
    .enumerate()
    .map(|(at, chunk)| Entry {
      name: format!("f{at}"),
      meta: NodeMeta {
        size: chunk.raw_len,
        ..NodeMeta::default()
      },
      node: Node::File(vec![Extent {
        offset: 0,
        len: chunk.raw_len,
        chunk: chunk.identity,
        chunk_offset: 0,
      }]),
    })
    .collect();
  Archive {
    manifest: Node::Directory(entries),
    chunks,
    ..replica_archive(BEFORE)
  }
}

/// Shape: the chunks of the cut-transfer placements — the fewest with a cut strictly inside the transfer
/// after more than one chunk (cuts after 0, 1, 2 and all 3 chunks).
const TRANSFER_CHUNKS: usize = 3;

/// The pieces of placement `tag`: [`TRANSFER_CHUNKS`] distinct chunks of one length, so placements with
/// different tags share no chunk and take equal charges.
fn tagged_pieces(tag: u8) -> Vec<Vec<u8>> {
  (0..TRANSFER_CHUNKS)
    .map(|at| {
      let mut piece = BEFORE.to_vec();
      piece.push(tag);
      piece.push(u8::try_from(at).unwrap());
      piece
    })
    .collect()
}

/// What the holder answered, as the owner reads it: the missing set's size, a progress reply, an
/// acknowledgement, or nothing.
#[derive(Debug, PartialEq, Eq)]
enum Answered {
  Missing(usize),
  Progress,
  Acked,
  Nothing,
}

fn answered(reply: &[u8]) -> Answered {
  use slates_cluster::content::ContentMessage;
  match ContentMessage::decode(reply) {
    Ok(ContentMessage::Missing { missing, .. }) => Answered::Missing(missing.len()),
    Ok(ContentMessage::Staged { .. }) => Answered::Progress,
    Ok(ContentMessage::Ack(_)) => Answered::Acked,
    _ => Answered::Nothing,
  }
}

/// AUD-29-55 (§4.9 "verified ranges and resumable progress"; AC-7.7, T-7.8): a transfer cut at any chunk
/// boundary keeps its verified chunks, never counts as placed, and resumes with exactly the chunks still owed.
/// Do: for every cut point `k` in 0..=N, offer a fresh N-chunk placement, send its first `k` chunks, re-offer,
/// send the rest, and re-offer once more (the acknowledgement lost at the cut). Expect each chunk before the
/// last answered with progress, the content not held while cut, the re-offer naming exactly N − k chunks, the
/// last chunk acknowledged, and the final re-offer acknowledged at once.
#[test]
fn a_cut_transfer_resumes_with_exactly_the_chunks_still_owed() {
  let profile = common::machine_profile();
  let instance = format!("srv-cut-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("cut", &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let runs: Vec<CutRun> = (0..=TRANSFER_CHUNKS)
    .map(|cut| cut_run(&daemon, cut))
    .collect();
  daemon.stop();
  drop(segment);
  for run in runs {
    assert_cut_run(&run);
  }
}

/// What one cut transfer drew from the holder, step by step.
struct CutRun {
  cut: usize,
  offered: Answered,
  before_cut: Vec<Answered>,
  held_while_cut: Result<bool, slates_server::observe::ObserveError>,
  resumed: Answered,
  after_cut: Vec<Answered>,
  held_after: Result<bool, slates_server::observe::ObserveError>,
  lost_ack: Answered,
}

/// Places a fresh [`TRANSFER_CHUNKS`]-chunk placement on `daemon`, cut after `cut` chunks, then resumed.
fn cut_run(daemon: &Daemon, cut: usize) -> CutRun {
  use slates_cluster::content::{chunk_requests, offer_request};
  use slates_db::register::{HostId, ObjectId};
  let serve = |request: Vec<u8>| answered(&daemon.serve_content_as_authorized(request).unwrap());
  let tag = u8::try_from(cut).unwrap();
  let archive = pieces_archive(&tagged_pieces(tag));
  let manifest = archive.manifest_identity();
  let object = ObjectId::new(HostId(1), u64::from(tag) + 1);
  let offered = serve(offer_request(&archive, object, 1));
  let identities: Vec<[u8; 32]> = archive.chunks.iter().map(|c| c.identity).collect();
  let requests = chunk_requests(&archive, object, 1, &identities);
  let before_cut = requests.iter().take(cut).cloned().map(serve).collect();
  let held_while_cut = daemon.fleet_holder_content(manifest);
  let resumed = serve(offer_request(&archive, object, 1));
  let after_cut = requests.iter().skip(cut).cloned().map(serve).collect();
  let held_after = daemon.fleet_holder_content(manifest);
  let lost_ack = serve(offer_request(&archive, object, 1));
  CutRun {
    cut,
    offered,
    before_cut,
    held_while_cut,
    resumed,
    after_cut,
    held_after,
    lost_ack,
  }
}

/// The replies `count` chunks draw when the last of them completes the placement or not: progress for each,
/// the acknowledgement for the completing one.
fn chunk_replies(count: usize, completes: bool) -> Vec<Answered> {
  (0..count)
    .map(|at| {
      if completes && at + 1 == count {
        Answered::Acked
      } else {
        Answered::Progress
      }
    })
    .collect()
}

/// The rule for one cut run (see the test's doc).
fn assert_cut_run(run: &CutRun) {
  let cut = run.cut;
  let complete_at_cut = cut == TRANSFER_CHUNKS;
  assert_eq!(run.offered, Answered::Missing(TRANSFER_CHUNKS), "cut {cut}");
  assert_eq!(
    run.before_cut,
    chunk_replies(cut, complete_at_cut),
    "cut {cut}"
  );
  assert_eq!(
    run.held_while_cut,
    Ok(complete_at_cut),
    "cut {cut}: incomplete content never counts as held"
  );
  let expected_resume = if complete_at_cut {
    Answered::Acked
  } else {
    Answered::Missing(TRANSFER_CHUNKS - cut)
  };
  assert_eq!(run.resumed, expected_resume, "cut {cut}: the re-offer");
  assert_eq!(
    run.after_cut,
    chunk_replies(TRANSFER_CHUNKS - cut, true),
    "cut {cut}"
  );
  assert_eq!(run.held_after, Ok(true), "cut {cut}");
  assert_eq!(
    run.lost_ack,
    Answered::Acked,
    "cut {cut}: a lost acknowledgement"
  );
}

/// AUD-29-55 (§4.9 "canceled producers neither publish partial identities nor leak"): an abandoned transfer
/// gives back everything it took when a newer placement replaces it. Do: place a control placement on one
/// object and measure its charge; then, on another, offer a placement of the same shape, cut it after one
/// chunk, and replace it with a newer placement of the same shape, completed. Expect the second object's
/// charge — bytes and index — equal to the control's (the abandoned stage left nothing), no stage left, and
/// the abandoned manifest not held.
#[test]
fn an_abandoned_transfer_returns_every_charge_when_replaced() {
  use slates_cluster::content::{chunk_requests, offer_request};
  use slates_db::register::{HostId, ObjectId};

  let profile = common::machine_profile();
  let instance = format!("srv-abandon-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("abandon", &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let account = || daemon.fleet_replica_account().unwrap();
  let start = account();
  let control = pieces_archive(&tagged_pieces(0));
  let control_object = ObjectId::new(HostId(1), 1);
  let control_reply = place_replica(&daemon, control_object, 1, &control);
  let after_control = account();

  let (abandoned, replacing) = (
    pieces_archive(&tagged_pieces(1)),
    pieces_archive(&tagged_pieces(2)),
  );
  let object = ObjectId::new(HostId(1), 2);
  let missing: Vec<[u8; 32]> = abandoned.chunks.iter().map(|c| c.identity).collect();
  let _ = daemon.serve_content_as_authorized(offer_request(&abandoned, object, 1));
  let first = chunk_requests(&abandoned, object, 1, &missing)
    .into_iter()
    .next()
    .expect("the abandoned placement has chunks");
  let cut_reply = answered(&daemon.serve_content_as_authorized(first).unwrap());
  let while_cut = account();
  let replaced_reply = place_replica(&daemon, object, 2, &replacing);
  let after = account();
  let abandoned_held = daemon.fleet_holder_content(abandoned.manifest_identity());
  daemon.stop();
  drop(segment);

  assert_eq!(answered(&control_reply), Answered::Acked);
  assert_eq!(cut_reply, Answered::Progress);
  assert_eq!(while_cut.stages, 1, "the cut transfer kept a stage");
  assert_eq!(answered(&replaced_reply), Answered::Acked);
  let charge = |from: &slates_server::daemon::ReplicaAccount,
                to: &slates_server::daemon::ReplicaAccount| {
    (to.replicated - from.replicated, to.index - from.index)
  };
  assert_eq!(
    charge(&after_control, &after),
    charge(&start, &after_control),
    "the replaced transfer left nothing charged beyond its replacement"
  );
  assert_eq!(after.stages, 0, "no stage is left");
  assert_eq!(after.replicated, after.charged);
  assert_eq!(abandoned_held, Ok(false));
}

/// Shape: the host memory the pressure test's first daemon reads at its first sample.
const SAMPLED_AVAILABLE: u64 = 8 << 30;
/// Shape: how much less the second daemon reads at its own first sample — a process that grew between
/// two daemons' starts (the fleet test binary grows by its leaked sockets and buffers).
const GROWN_BETWEEN_STARTS: u64 = 1 << 30;
/// Shape: a further drop the second daemon reads later, which is real pressure on it.
const LATER_DROP: u64 = 256 << 20;
/// Shape: how long the test waits for a sampler to act — many liveness cadences (one second each).
const SAMPLER_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Shape: the pause between the pressure test's polls — a twentieth of the sampler's one-second cadence.
const SAMPLER_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Polls `done` until it holds or [`SAMPLER_WAIT`] passes.
fn within_sampler_wait(mut done: impl FnMut() -> bool) -> bool {
  let began = std::time::Instant::now();
  while began.elapsed() < SAMPLER_WAIT {
    if done() {
      return true;
    }
    // The harness paces its poll of a sampler that acts once a second; shipped code parks on its
    // driver (D-9).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(SAMPLER_POLL);
  }
  done()
}

/// §4.2, admission.md §5.5 (found by the AUD-29-43 churn test's CI failures). Do: start one daemon whose
/// sampler reads a host's available memory; start a second in the same process that reads 1 GiB less at
/// its first sample; then let the second read a further 256 MiB less. Expect: neither daemon holds
/// anything back for the difference between their starts — each measures pressure from its own first
/// sample — and the second's later drop is held back across its shards. Before the fix the baseline was
/// process-wide, fixed by whichever daemon sampled first, so the second daemon withheld the first's
/// growth as if it were pressure.
#[test]
fn two_daemons_in_one_process_measure_memory_pressure_from_their_own_start() {
  let profile = common::machine_profile();
  let start = |name: &str| {
    let instance = format!("srv-pressure-{name}-{}", std::process::id());
    let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
    let segment = anchor_segment(&format!("pressure-{name}"), &profile, &config);
    let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
    (daemon, segment)
  };
  let (first, first_segment) = start("first");
  // The process's resident memory is pinned too: the hold subtracts the daemon's own resident growth, and a real
  // reading of a test process (both daemons and the harness) moves with whatever else runs, which made this test fail
  // under x86_64 emulation at load 60 (2026-10-05).
  first.inject_resident_memory(SAMPLED_RESIDENT).unwrap();
  first.inject_available_memory(SAMPLED_AVAILABLE).unwrap();
  let first_sampled =
    within_sampler_wait(|| first.pressure_baseline() == Ok(Some(SAMPLED_AVAILABLE)));
  let (second, second_segment) = start("second");
  second.inject_resident_memory(SAMPLED_RESIDENT).unwrap();
  second
    .inject_available_memory(SAMPLED_AVAILABLE - GROWN_BETWEEN_STARTS)
    .unwrap();
  let second_sampled = within_sampler_wait(|| {
    second.pressure_baseline() == Ok(Some(SAMPLED_AVAILABLE - GROWN_BETWEEN_STARTS))
  });
  let second_hold_at_start = second.pressure_hold();
  second
    .inject_available_memory(SAMPLED_AVAILABLE - GROWN_BETWEEN_STARTS - LATER_DROP)
    .unwrap();
  let per_shard = LATER_DROP / u64::from(TEST_SHARDS);
  let pressure_held = within_sampler_wait(|| second.pressure_hold() == Ok(per_shard));
  let first_hold = first.pressure_hold();
  first.stop();
  second.stop();
  drop((first_segment, second_segment));
  assert!(first_sampled, "the first daemon took its first sample");
  assert!(
    second_sampled,
    "the second daemon's baseline is its own first sample, not the first daemon's"
  );
  assert_eq!(
    second_hold_at_start,
    Ok(0),
    "the second daemon withholds nothing for the process's growth before it started"
  );
  assert!(
    pressure_held,
    "a later drop on the second daemon is held back, divided among its shards"
  );
  assert_eq!(
    first_hold,
    Ok(0),
    "the first daemon saw no pressure of its own"
  );
}

/// Shape: the pressure test's daemon's own resident memory at its first sample.
const SAMPLED_RESIDENT: u64 = 64 << 20;
/// Shape: the bytes the daemon itself takes after its first sample (content it stored): the host's available memory
/// falls by them and its own resident memory rises by them.
const OWN_GROWTH: u64 = 512 << 20;

/// §4.2, admission.md §5.5 (found 2026-10-05 in a 1 GiB container: one volume refused at 244 MB with a hold of 111 MB
/// per shard for memory the daemon had itself filled). Do: let a daemon's sampler read a host's available memory and
/// its own resident memory; then let available fall by [`OWN_GROWTH`] while its resident memory rises by the same;
/// then let available fall a further [`LATER_DROP`] with its resident memory unchanged. Expect: no hold for its own
/// growth (that is committed content, charged already), and the further drop held back across its shards. Before the
/// fix the hold counted the daemon's own growth as pressure, so every stored byte was charged twice and one volume
/// reached about half the content capacity.
#[test]
fn a_daemons_own_growth_is_not_memory_pressure() {
  let profile = common::machine_profile();
  let instance = format!("srv-pressure-own-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("pressure-own", &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  daemon.inject_available_memory(SAMPLED_AVAILABLE).unwrap();
  daemon.inject_resident_memory(SAMPLED_RESIDENT).unwrap();
  let sampled = within_sampler_wait(|| daemon.pressure_baseline() == Ok(Some(SAMPLED_AVAILABLE)));
  daemon
    .inject_resident_memory(SAMPLED_RESIDENT + OWN_GROWTH)
    .unwrap();
  daemon
    .inject_available_memory(SAMPLED_AVAILABLE - OWN_GROWTH)
    .unwrap();
  // Two sampler periods: the hold a wrong reading would set has had its chance to appear.
  let wrongly_held = within_sampler_wait(|| daemon.pressure_hold().is_ok_and(|hold| hold > 0));
  daemon
    .inject_available_memory(SAMPLED_AVAILABLE - OWN_GROWTH - LATER_DROP)
    .unwrap();
  let per_shard = LATER_DROP / u64::from(TEST_SHARDS);
  let pressure_held = within_sampler_wait(|| daemon.pressure_hold() == Ok(per_shard));
  let hold = daemon.pressure_hold();
  daemon.stop();
  assert!(sampled, "the daemon took its first sample");
  assert!(!wrongly_held, "its own growth is not held back as pressure");
  assert!(
    pressure_held,
    "a drop it did not cause is held back, divided among its shards: {hold:?}"
  );
}

/// AUD-29-43 (§4.2 "a remote holder makes the same admission against its own machine before acknowledging
/// placement"): a replica is admitted only from the holder's unpromised capacity. Do: withhold the whole of
/// every shard's admittable capacity (the memory-pressure hold), put a replica, then release the hold and
/// put it again. Expect: the first put refused (no acknowledgement) with nothing held, and the second
/// acknowledged and held. Before the fix the hold charged nothing and acknowledged whatever arrived.
#[test]
fn a_replica_is_admitted_only_from_the_holders_unpromised_capacity() {
  use slates_cluster::content::ContentMessage;
  use slates_db::register::{HostId, ObjectId};

  let profile = common::machine_profile();
  let instance = format!("srv-replica-cap-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("replica-cap", &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let archive = replica_archive(BEFORE);
  let manifest = archive.manifest_identity();
  let object = ObjectId::new(HostId(1), 1);
  let acked = |reply: &[u8]| matches!(ContentMessage::decode(reply), Ok(ContentMessage::Ack(_)));

  daemon.inject_pressure_hold(u64::MAX).unwrap();
  let refused = place_replica(&daemon, object, 1, &archive);
  let held_while_full = daemon.fleet_holder_content(manifest);
  daemon.inject_pressure_hold(0).unwrap();
  let admitted = place_replica(&daemon, object, 1, &archive);
  let held_after = daemon.fleet_holder_content(manifest);
  daemon.stop();
  drop(segment);
  assert!(
    !acked(&refused),
    "no acknowledgement without unpromised capacity"
  );
  assert_eq!(
    held_while_full,
    Ok(false),
    "nothing was held while the capacity was withheld"
  );
  assert!(
    acked(&admitted),
    "the put is acknowledged once capacity returns"
  );
  assert_eq!(held_after, Ok(true));
}

/// AUD-29-59 (§4.8 persistence before reply; §4.10 placement closure): a holder acknowledges a content put
/// only for content its anchor-owned RAM retains, so a warm restart keeps every acknowledged replica. Do:
/// place a replica on a daemon through the holder's production path, stop it, and start a second daemon over
/// the same anchor segment and content object. Expect: the put acknowledged, and the replica held whole after
/// the restart. Before the fix the hold lived only in process memory and every restart began it empty, so an
/// acknowledgement that placement counted was gone.
#[test]
fn an_acknowledged_replica_survives_a_warm_daemon_restart() {
  use slates_cluster::content::ContentMessage;
  use slates_db::register::{HostId, ObjectId};

  let profile = common::machine_profile();
  let instance = format!("srv-replica-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("replica", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");

  let archive = replica_archive(BEFORE);
  let manifest = archive.manifest_identity();
  let reply = place_replica(&first, ObjectId::new(HostId(1), 1), 1, &archive);
  let acknowledged = matches!(ContentMessage::decode(&reply), Ok(ContentMessage::Ack(_)));
  let held_before = first.fleet_holder_content(manifest);
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let held_after = second.fleet_holder_content(manifest);
  second.stop();
  drop(segment);
  assert!(acknowledged, "the holder acknowledged the put");
  assert_eq!(
    held_before,
    Ok(true),
    "the replica was held before the restart"
  );
  assert_eq!(
    held_after,
    Ok(true),
    "the acknowledged replica is held after a warm restart"
  );
}

// -------------------------------------------- crash injection at every durable step (AC-2.3, AC-2.12)
//
// A scenario of single-transaction steps, so each crash point is a real state a kill can leave. Two
// crash states need no surgery — stop the first daemon *before* the step (nothing durable) or *after*
// it (both the content publish and the log record durable); the same segment and content object carry
// through to the second daemon (the client-restart pattern). The third state — published, but crashed
// before the log record committed — is produced by rolling the log ring's tail back to before the
// step (the content object keeps the step's published image, since a control verb publishes in its
// dispatch before the completion transaction commits). Rolling back the tail is a 64-byte header
// write; the log region itself is many gigabytes (a whole recovery budget of records) and is never
// copied.

/// Format: the width of a log record's body-length field and where it sits — a `u32` at offset 4 of
/// the 32-byte record header (`crates/db/src/record.rs`: magic, then the little-endian body length).
/// The oracle reads it to find one record's end so it can roll the ring back to exactly there.
const RECORD_LEN_AT: usize = 4;

/// Shape: the volume's size class at creation, and after the scenario's resize (bytes).
const CREATED_LIMIT: u64 = 1 << 20;
const RESIZED_LIMIT: u64 = 2 << 20;
/// Shape: the scenario's steps (the numbered arms of [`step`]).
const STEPS: usize = 6;
/// Shape: the steps that are control verbs — a publish then a completion record, so three crash states
/// each; the rest are mount-transport mutations — a publish only, so two.
const CONTROL_STEPS: [usize; 3] = [1, 4, 6];
/// Shape: the crash points the sweep yields — three per control step, two per mount mutation.
const CRASH_POINTS: usize = CONTROL_STEPS.len() * 3 + (STEPS - CONTROL_STEPS.len()) * 2;

/// Where a crash falls inside one step's durable writes, in the daemon's order — the shard image is
/// published, then (for a control verb) the completion record commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Crash {
  /// Before the publish: nothing of the step reached anchor-owned RAM (the first daemon stops before
  /// running the step).
  BeforePublish,
  /// After the publish, before the record: the content image is ahead of the log (the log ring's tail
  /// is rolled back over the step's record). A control verb only — a mount mutation has no record.
  BetweenPublishAndRecord,
  /// After both: the step is acknowledged; a retry meets its completion record.
  AfterRecord,
}

/// Reads a ring's tail (its monotonic byte write offset) from the log region's tail word.
fn log_tail(segment: &AnchorSegment, partition: u16) -> u64 {
  segment.ring_words(RegionKind::Log(partition)).unwrap()[1].load(Ordering::Acquire)
}

/// The header words (head, tail, capacity, sequence base) of each partition's log ring — what a
/// crash-before-the-record rolls back. The words, never the many-gigabyte data ring.
fn log_headers(segment: &AnchorSegment, partitions: u16) -> Vec<(u16, Vec<u64>)> {
  (0..partitions)
    .map(|p| {
      let words = segment.ring_words(RegionKind::Log(p)).unwrap();
      (
        p,
        words
          .iter()
          .map(|word| word.load(Ordering::Acquire))
          .collect(),
      )
    })
    .collect()
}

/// Puts each ring's header words back, rolling its tail (and head/sequence) to the captured state — so
/// a record appended after the capture is beyond the tail and ignored on replay, exactly as a crash
/// before that record's durable commit would leave it.
fn restore_log_headers(segment: &mut AnchorSegment, headers: &[(u16, Vec<u64>)]) {
  for (p, values) in headers {
    let words = segment.ring_words(RegionKind::Log(*p)).unwrap();
    for (word, value) in words.iter().zip(values) {
      word.store(*value, Ordering::Release);
    }
  }
}

/// One scenario in flight: the client, what it holds, and the request id of each control step so a
/// resume of an acknowledged step retries under its original id (the exactly-once path).
struct Run {
  client: Client,
  kept: Option<VolumeId>,
  head_file: Option<Vec<u8>>,
  requests: Vec<Option<RequestId>>,
  xid: u32,
}

impl Run {
  fn new(client: Client) -> Run {
    Run {
      client,
      kept: None,
      head_file: None,
      requests: vec![None; STEPS + 1],
      xid: 0,
    }
  }

  fn xid(&mut self) -> u32 {
    self.xid += 1;
    self.xid
  }

  /// Runs a control verb. `retry` replays it under the id it first ran with (an acknowledged step,
  /// whose completion record answers); otherwise it is a fresh call under the next sequence — which is
  /// what a step that never committed (before-publish, or a rolled-back between-state) resumes as.
  fn control(&mut self, k: usize, retry: bool, body: &RequestBody) -> ReplyBody {
    let reply = if retry {
      let id = self.requests[k].expect("the step ran before");
      self.client.retry(id, body).unwrap()
    } else {
      let reply = self.client.call(body).unwrap();
      self.requests[k] = Some(self.client.last_request());
      reply
    };
    if let ReplyBody::Created { id } = reply {
      self.kept = Some(id);
    }
    reply
  }
}

/// The NFS root handle of `/<volume>` on `daemon`, with its connection.
fn mount_volume(run: &mut Run, daemon: &Daemon, volume: &str) -> (TcpStream, Vec<u8>) {
  let mut stream = nfs_stream(daemon);
  let xid = run.xid();
  let root = mount(&mut stream, &capability_path(daemon, volume), xid);
  (stream, root)
}

/// The scenario's step `k` on `daemon`: a control verb through the client (replayed under its original
/// id when `retry`), or a mutation over the mount transport (re-issued as is — a CREATE is UNCHECKED
/// and a WRITE at an offset is idempotent, which is how an NFS client resumes after a server restart).
fn step(run: &mut Run, daemon: &Daemon, k: usize, retry: bool) {
  match k {
    1 => {
      let body = RequestBody::Create {
        name: "kept".to_owned(),
        size: SizeClass::Bounded {
          limit: CREATED_LIMIT,
        },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      };
      match run.control(k, retry, &body) {
        ReplyBody::Created { .. } => {}
        other => panic!("create: {other:?}"),
      }
    }
    2 => {
      let (mut stream, root) = mount_volume(run, daemon, "kept");
      let xid = run.xid();
      run.head_file = Some(create(&mut stream, &root, "f", xid));
    }
    3 | 5 => {
      let bytes = if k == 3 { BEFORE } else { AFTER };
      let mut stream = nfs_stream(daemon);
      let file = run.head_file.clone().expect("f was created");
      let xid = run.xid();
      write(&mut stream, &file, bytes, xid);
    }
    4 => {
      let body = RequestBody::Snapshot {
        volume: run.kept.expect("kept exists"),
      };
      match run.control(k, retry, &body) {
        ReplyBody::Snapshotted { .. } => {}
        other => panic!("snapshot: {other:?}"),
      }
    }
    6 => {
      let body = RequestBody::Resize {
        volume: run.kept.expect("kept exists"),
        size: SizeClass::Bounded {
          limit: RESIZED_LIMIT,
        },
      };
      assert_eq!(run.control(k, retry, &body), ReplyBody::Resized);
    }
    _ => panic!("no step {k}"),
  }
}

/// The observable end state, compared whole against the reference: the kept volume's snapshot count,
/// its file's bytes, and its acknowledged capacity (the quota the mount reports).
#[derive(Debug, PartialEq, Eq)]
struct Observed {
  snapshots: u32,
  file: Vec<u8>,
  capacity: u64,
}

fn observe(run: &mut Run, daemon: &Daemon) -> Observed {
  let kept = run.kept.expect("kept exists");
  let snapshots = run.client.status(kept).unwrap().snapshots;
  let (mut stream, root) = mount_volume(run, daemon, "kept");
  let xid = run.xid();
  let (capacity, _) = fsstat(&mut stream, &root, xid);
  Observed {
    snapshots,
    file: read_file(daemon, "kept", "f"),
    capacity,
  }
}

/// What the recovered daemon must show for the crash state itself, before the resume: the catalog's
/// acknowledged state and nothing of an unacknowledged effect. The snapshot check uses D-13's exact
/// counters — an image-only snapshot the trim removed would leave a version sharing the head's bytes
/// (unique < referenced); its absence proves the trim. The capacity check proves the acknowledged
/// quota was restored over the image's.
fn assert_crash_state(run: &mut Run, daemon: &Daemon, k: usize, done: bool) {
  match k {
    1 => {
      let has_kept = run.client.list().unwrap().iter().any(|v| v.name == "kept");
      assert_eq!(
        has_kept, done,
        "crash at step 1: kept exists iff acknowledged"
      );
    }
    4 => {
      let report = run.client.status(run.kept.unwrap()).unwrap();
      if done {
        assert_eq!(report.snapshots, 1, "crash at step 4 after record");
      } else {
        assert_eq!(
          report.snapshots, 0,
          "crash at step 4: no unrecorded snapshot"
        );
        assert!(report.referenced_bytes > 0, "the head has content");
        assert_eq!(
          report.unique_bytes, report.referenced_bytes,
          "crash at step 4: no hidden snapshot shares the head's bytes (image-only snapshot trimmed)"
        );
      }
    }
    6 => {
      let (mut stream, root) = mount_volume(run, daemon, "kept");
      let xid = run.xid();
      let (capacity, _) = fsstat(&mut stream, &root, xid);
      assert_eq!(
        capacity,
        if done { RESIZED_LIMIT } else { CREATED_LIMIT },
        "crash at step 6: the acknowledged size policy, not the image's"
      );
    }
    _ => {}
  }
}

/// The whole scenario once on a fresh daemon with no crash — the reference every resume must reach.
/// Built from its own clean run, never from a crashed state.
fn reference(profile: &MachineProfile) -> Observed {
  let instance = format!("srv-crash-ref-{}", std::process::id());
  let config = DaemonConfig::derive(profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("crash-ref", profile, &config);
  let daemon = Daemon::start(profile, config, source_of(&segment)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut run = Run::new(connect(&instance));
  for k in 1..=STEPS {
    step(&mut run, &daemon, k, false);
  }
  let observed = observe(&mut run, &daemon);
  daemon.stop();
  drop(segment);
  observed
}

/// AC-2.3 / AC-2.12 (T-2.14): crash injection at every durable step of the recovery path with a resume
/// that must reach the reference. Each step writes into anchor-owned RAM in the daemon's order — the
/// shard image published, then (a control verb) the completion record committed. The test, as the
/// anchor, starts a second daemon over each crash state a kill at that step can leave: before the
/// publish (stop before the step), between the publish and the record (roll the log's tail back over
/// the step's record, the content image kept), and after both (stop after the step). A mount mutation
/// has no record, so two states. The recovered daemon must show the catalog's acknowledged state and
/// nothing of an unacknowledged effect; then the client resumes — an acknowledged step replayed under
/// its original id, an unacknowledged one re-issued — and finishes the scenario, whose end state must
/// equal a separate clean run's. A file handle minted before a crash must still resolve after it.
/// Non-vacuous: without the image-only snapshot trim, the between-state at step 4 shows a hidden
/// snapshot (unique ≠ referenced bytes); without the acknowledged-quota revert, the between-state at
/// step 6 shows the image's larger capacity.
#[test]
fn a_crash_at_every_durable_step_recovers_and_the_resume_reaches_the_reference() {
  let profile = common::machine_profile();
  let reference = reference(&profile);
  let mut points = 0;
  for k in 1..=STEPS {
    let crashes: &[Crash] = if CONTROL_STEPS.contains(&k) {
      &[
        Crash::BeforePublish,
        Crash::BetweenPublishAndRecord,
        Crash::AfterRecord,
      ]
    } else {
      &[Crash::BeforePublish, Crash::AfterRecord]
    };
    for &crash in crashes {
      points += 1;
      let started = Instant::now();
      run_crash_point(&profile, &reference, k, crash);
      eprintln!(
        "recovery oracle: crash point {points}/{CRASH_POINTS} (step {k}, {crash:?}) in {} ms",
        started.elapsed().as_millis()
      );
    }
  }
  assert_eq!(points, CRASH_POINTS, "every crash point ran");
}

/// One crash point: run the scenario to step `k` on a first daemon, leave the crash state a kill at
/// that step would (before the publish, between the publish and the record, or after both), start a
/// second daemon over it, assert the recovered state, resume, and require the end state to equal the
/// clean `reference`.
fn run_crash_point(profile: &MachineProfile, reference: &Observed, k: usize, crash: Crash) {
  let name = format!("crash-{k}-{crash:?}");
  let instance = format!("srv-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(profile, &instance, Some(TEST_SHARDS));
  let mut segment = anchor_segment(&name, profile, &config);
  let partitions = config.geometry.partitions;

  let first = Daemon::start(profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut run = Run::new(connect(&instance));
  for j in 1..k {
    step(&mut run, &first, j, false);
  }
  let before = log_headers(&segment, partitions);
  // Before-publish: the step never runs on the first daemon; the others run it, then the
  // between-state rolls the log back so only the content publish survives.
  if crash != Crash::BeforePublish {
    step(&mut run, &first, k, false);
  }
  first.stop();
  if crash == Crash::BetweenPublishAndRecord {
    restore_log_headers(&mut segment, &before);
  }

  let second = Daemon::start(profile, config, source_of(&segment)).unwrap();
  let done = crash == Crash::AfterRecord;
  assert_crash_state(&mut run, &second, k, done);
  step(&mut run, &second, k, done);
  for j in k + 1..=STEPS {
    step(&mut run, &second, j, false);
  }
  assert_eq!(
    observe(&mut run, &second),
    *reference,
    "crash {crash:?} at step {k}: the resume reaches the reference"
  );
  let mut stream = nfs_stream(&second);
  let xid = run.xid();
  assert_eq!(
    read(&mut stream, run.head_file.as_ref().unwrap(), xid),
    AFTER,
    "crash {crash:?} at step {k}: a handle minted before the crash resolves after it"
  );
  drop(stream);
  second.stop();
  drop(segment);
}

// ----------------------------------------------------------- the landing counter across a restart

/// AC-2.12 (§4.8 recovery; §4.15 landings): a landing presented before a restart does not block the
/// first landing after it. The first daemon presents a landing (no grant — `GrantRequired`, its record
/// durable); a second daemon over the same segment presents another — it must be `GrantRequired` under
/// a **new** landing id, not refused: the landing counter boots past the recovered records
/// (`docs/bugs/2026-09-19-landing-counter-restarts-at-one-after-a-restart.md`). Non-vacuous: before
/// the fix the second present re-minted the recovered record's id and was refused `AlreadyExists`.
#[test]
fn a_landing_presented_before_a_restart_does_not_block_the_first_landing_after_it() {
  let profile = common::machine_profile();
  let instance = format!("srv-landctr-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("landctr", &profile, &config);
  let target = common::target::target_dir();

  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let kept = client.create(&scratch("kept")).unwrap();
  let snapshot = client.snapshot(kept).unwrap();
  let before = present_landing(&mut client, kept, snapshot, &target.path);
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let after = present_landing(&mut client, kept, snapshot, &target.path);
  assert_ne!(
    after, before,
    "the landing presented after the restart is a new landing, past the recovered record"
  );
  second.stop();
  drop(target);
  drop(segment);
}

/// Shape: the grant term of the restart test's approvals — a minute, far past the test's length.
const GRANT_TERM_NS: u64 = 60_000_000_000;

/// Presents a landing of `snapshot` into `target` and approves it under `scope`, as the human's surface does
/// (the proof under the daemon's issuer secret); the grant id.
fn approve(
  client: &mut Client,
  secret: &[u8; 32],
  volume: VolumeId,
  snapshot: SnapshotId,
  target: &str,
  scope: slates_ipc::protocol::GrantScope,
) -> u64 {
  let presented = client.land(volume, Some(snapshot), target, Filter::default(), None);
  let Ok(slates_client::Landing::GrantRequired {
    landing, manifest, ..
  }) = presented
  else {
    panic!("the landing was not presented: {presented:?}");
  };
  let proof = slates_server::landing::grant_proof(secret, landing, &manifest, scope, GRANT_TERM_NS);
  client
    .grant(landing, manifest, scope, GRANT_TERM_NS, proof)
    .expect("the approval issues a grant")
}

/// AUD-29-06 (§4.15 step 3: a session grant covers later landings for its session; §4.8 recovery): a session
/// grant covers its binding across a daemon restart, a single-use grant stays spent across one, and a grant
/// issued after a restart takes an id no earlier grant had. Before 2026-09-29 a restarted shard rebuilt no
/// runtime grant (every grant was lost at a restart, and the first grant after it re-minted a recorded id,
/// refused `AlreadyExists`), and every landing recorded its grant consumed, a session grant included. Do:
/// land under a single-use grant; approve a session grant for another volume; restart the daemon over the
/// same segment; land twice under the session grant; list the grants; approve a new landing. Expect: both
/// session landings land; the single-use grant is listed consumed and the session grant issued; the new
/// grant's id is neither earlier one.
#[test]
fn a_session_grant_outlives_a_restart_and_a_single_use_grant_stays_spent() {
  use slates_ipc::protocol::GrantScope;
  let profile = common::machine_profile();
  let instance = format!("srv-grantrst-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("grantrst", &profile, &config);
  let target = common::target::target_dir();

  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let secret = first.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let spent = client.create(&scratch("spent")).unwrap();
  let spent_snapshot = client.snapshot(spent).unwrap();
  let once = approve(
    &mut client,
    &secret,
    spent,
    spent_snapshot,
    &target.path,
    GrantScope::Once,
  );
  let landed = client.land(
    spent,
    Some(spent_snapshot),
    &target.path,
    Filter::default(),
    Some(once),
  );
  assert!(
    matches!(landed, Ok(slates_client::Landing::Landed(_))),
    "{landed:?}"
  );
  let kept = client.create(&scratch("kept")).unwrap();
  let kept_snapshot = client.snapshot(kept).unwrap();
  let session = approve(
    &mut client,
    &secret,
    kept,
    kept_snapshot,
    &target.path,
    GrantScope::Session,
  );
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  for round in 0..2 {
    let landed = client.land(
      kept,
      Some(kept_snapshot),
      &target.path,
      Filter::default(),
      Some(session),
    );
    assert!(
      matches!(landed, Ok(slates_client::Landing::Landed(_))),
      "round {round} under the session grant after the restart: {landed:?}"
    );
  }
  let states: std::collections::BTreeMap<u64, String> = client
    .grants()
    .unwrap()
    .into_iter()
    .map(|grant| (grant.id, grant.state))
    .collect();
  assert_eq!(
    states.get(&once).map(String::as_str),
    Some("consumed"),
    "{states:?}"
  );
  assert_eq!(
    states.get(&session).map(String::as_str),
    Some("issued"),
    "{states:?}"
  );
  let fresh = client.create(&scratch("fresh")).unwrap();
  let fresh_snapshot = client.snapshot(fresh).unwrap();
  let renewed = approve(
    &mut client,
    &second.segment().issuer_secret().unwrap(),
    fresh,
    fresh_snapshot,
    &target.path,
    GrantScope::Once,
  );
  assert!(
    renewed != once && renewed != session,
    "the grant after the restart re-minted an earlier id: {renewed} against {once} and {session}"
  );
  second.stop();
  drop(target);
  drop(segment);
}

/// Presents a landing of `snapshot` into `target` with no grant and returns its landing id — the
/// `GrantRequired` reply; anything else (a refusal) is the failure the test names.
fn present_landing(
  client: &mut Client,
  volume: VolumeId,
  snapshot: SnapshotId,
  target: &str,
) -> u64 {
  match client.call(&RequestBody::Land {
    volume,
    snapshot: Some(snapshot),
    target: target.to_owned(),
    filter: Filter::default(),
    grant: None,
  }) {
    Ok(ReplyBody::GrantRequired { landing, .. }) => landing,
    other => panic!("the landing was not presented: {other:?}"),
  }
}

// ----------------------------------------------------------- target landing leases across a restart

/// Shape: the long lease of the restart test — a minute, far past the test.
const MINUTE_LEASE_NS: u64 = 60_000_000_000;
/// Shape: the attempt the restart test's leases stand for: no landing attempt of the daemon's is numbered
/// this high (its counters start at one under a sixteen-bit partition).
const OTHER_ATTEMPT: u64 = u64::MAX;
/// Shape: the pause between the restart test's reads of the clock while it waits out a lease's term.
const LEASE_POLL: Duration = Duration::from_millis(10);

/// A landing granted for the session: its volume (a fresh one named `name`), snapshot, target and grant.
type SessionLanding = (VolumeId, SnapshotId, String, u64);

/// Grants, for the session, the landing of a fresh volume named `name` into `target`.
fn session_landing(
  client: &mut Client,
  secret: &[u8; 32],
  name: &str,
  target: &str,
) -> SessionLanding {
  let volume = client.create(&scratch(name)).unwrap();
  let snapshot = client.snapshot(volume).unwrap();
  let grant = approve(
    client,
    secret,
    volume,
    snapshot,
    target,
    slates_ipc::protocol::GrantScope::Session,
  );
  (volume, snapshot, target.to_owned(), grant)
}

/// Lands `landing` under its grant.
fn land_granted(
  client: &mut Client,
  landing: &SessionLanding,
) -> Result<slates_client::Landing, ClientError> {
  let (volume, snapshot, target, grant) = landing;
  client.land(
    *volume,
    Some(*snapshot),
    target,
    Filter::default(),
    Some(*grant),
  )
}

/// Waits until the host's monotonic clock has passed `expires_ns`.
fn wait_past(expires_ns: u64) {
  while slates_machine::clock::monotonic_ns() <= expires_ns {
    // The harness paces its wait for a term to end; shipped code parks on its driver (D-9).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(LEASE_POLL);
  }
}

/// The target leases the daemon records, over every shard.
fn target_leases(client: &mut Client) -> u64 {
  client
    .daemon_status()
    .unwrap()
    .shards
    .iter()
    .map(|shard| shard.target_leases)
    .sum()
}

/// AUD-29-03 (§4.15 step 4 and "Ownership facts"; §4.8 recovery — restart and cancellation): a target lease
/// is a durable record of the control partition, so it outlives the daemon that took it — an attempt its
/// daemon's end cancels leaves its lease to its term — and a recovered lease blocks landings until that term
/// ends, and not after. Do: under a first daemon, grant for the session the landings of two volumes into two
/// targets, then take each target's lease for another attempt — the first for a minute, the second for the
/// recovery budget — and stop the daemon; under a second daemon over the same segment, land both, the second
/// once its lease's term has passed. Expect: the first landing is refused `LandingLeaseHeld` naming that
/// attempt, with its target empty; the second lands; and the minute lease is the only record left — the
/// landing released its own and its take released the ended one.
#[test]
fn a_target_lease_outlives_its_daemon_and_blocks_landings_until_its_term() {
  use common::lease::{hold, lease_key_of};
  let profile = common::machine_profile();
  let instance = format!("srv-leaserst-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("leaserst", &profile, &config);
  let blocked = common::target::target_dir();
  let freed = common::target::target_dir();

  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let secret = first.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let into_blocked = session_landing(&mut client, &secret, "blocked", &blocked.path);
  let into_freed = session_landing(&mut client, &secret, "freed", &freed.path);
  hold(
    &first,
    &lease_key_of(&blocked.path),
    OTHER_ATTEMPT,
    MINUTE_LEASE_NS,
  )
  .unwrap();
  let ending = hold(
    &first,
    &lease_key_of(&freed.path),
    OTHER_ATTEMPT,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .unwrap();
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let refused = land_granted(&mut client, &into_blocked);
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(
        slates_ipc::protocol::Refusal::LandingLeaseHeld {
          holder: OTHER_ATTEMPT
        }
      ))
    ),
    "the lease taken before the restart holds the target after it: {refused:?}"
  );
  assert_eq!(std::fs::read_dir(&blocked.path).unwrap().count(), 0);
  wait_past(ending.expires_ns);
  let landed = land_granted(&mut client, &into_freed);
  assert!(
    matches!(landed, Ok(slates_client::Landing::Landed(_))),
    "a recovered lease past its term blocks nothing: {landed:?}"
  );
  assert_eq!(target_leases(&mut client), 1, "the minute lease alone");
  second.stop();
  drop((blocked, freed));
  drop(segment);
}

// ------------------------------------------------ clone pins and destroy completion across a restart

/// The partition that owns `volume` (`slates_server::verbs::owner_of` over its id's bytes).
fn owner_partition(volume: VolumeId) -> u16 {
  slates_server::verbs::owner_of(slates_ipc::protocol::VolumeId {
    bytes: volume.bytes,
  })
}

/// AC-2.12 (§4.8 recovery authority): a clone's pin on its origin snapshot, and a destroy still in
/// flight, are reconciled to the catalog across a restart. A clone is created (pinning the snapshot),
/// then the daemon is restarted; the recovered snapshot must still be pinned — its destroy refused —
/// because `reconcile_clone_pins` restored the pin the image carried against the recorded clone. Then
/// the clone's destroy verb is committed and the daemon is restarted *before its background
/// reclamation ran* — the state a crash leaves, reproduced by rolling the log's tail to just past the
/// destroy verb's record (excluding the later `VolumeDestroyed`), so the catalog says `Destroying`;
/// `complete_recovered_destroys` finishes it at boot, unpinning the origin, and the snapshot's destroy
/// then succeeds. Non-vacuous: without the pin reconciliation the first destroy-snapshot would succeed
/// (the image's pin lost); without the destroy completion the clone would stay `Destroying` forever
/// and the snapshot pinned.
#[test]
fn a_clone_pin_and_a_destroy_in_flight_reconcile_to_the_catalog_across_a_restart() {
  let profile = common::machine_profile();
  let instance = format!("srv-pins-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let mut segment = anchor_segment("pins", &profile, &config);

  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let kept = client.create(&scratch("kept")).unwrap();
  let snapshot = client.snapshot(kept).unwrap();
  let clone = client.clone_snapshot(kept, snapshot, "kept-clone").unwrap();
  first.stop();

  // Restart 1: the clone's pin on the snapshot survived in the image and is reconciled to the recorded
  // clone, so the snapshot's destroy is refused (a lost pin would be the only reason it were allowed).
  let second = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  assert!(
    matches!(
      client.destroy_snapshot(kept, snapshot),
      Err(ClientError::Refused(slates_ipc::protocol::Refusal::BadRequest { reason })) if reason == "Pinned"
    ),
    "the recovered snapshot is still pinned by the recovered clone (a lost pin would let it be destroyed): {:?}",
    client.destroy_snapshot(kept, snapshot)
  );

  // Destroy the clone; roll the log back to just past the destroy verb's record so the catalog shows
  // `Destroying` with the background `VolumeDestroyed` never durable — the crash a mid-reclamation kill
  // leaves. The destroy verb commits `Destroying` + the completion in one record; the next record is
  // the reclamation's `VolumeDestroyed`.
  let clone_partition = owner_partition(clone);
  let before_destroy = log_tail(&segment, clone_partition);
  client.destroy(clone).unwrap();
  // Wait for the background reclamation to run (so a `VolumeDestroyed` record exists after the destroy
  // verb's), then cut the log to before it.
  let started = Instant::now();
  loop {
    let listed: Vec<String> = client.list().unwrap().into_iter().map(|v| v.name).collect();
    if listed == ["kept"] {
      break;
    }
    assert!(
      started.elapsed() < Duration::from_secs(5),
      "the clone's destroy reclamation runs: {listed:?}"
    );
  }
  cut_log_after_one_record(&mut segment, clone_partition, before_destroy);
  second.stop();

  // Restart 2 over the `Destroying` state: recovery completes the destroy, unpinning the origin, so
  // the snapshot's destroy now succeeds.
  let third = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let started = Instant::now();
  loop {
    let listed: Vec<String> = client.list().unwrap().into_iter().map(|v| v.name).collect();
    if listed == ["kept"] {
      break;
    }
    assert!(
      started.elapsed() < Duration::from_secs(5),
      "the in-flight clone destroy completes on recovery: {listed:?}"
    );
  }
  client.destroy_snapshot(kept, snapshot).unwrap();
  assert_eq!(
    client.status(kept).unwrap().snapshots,
    0,
    "the snapshot's destroy succeeds once the recovered destroy unpinned it"
  );
  third.stop();
  drop(segment);
}

/// Rolls a partition's log ring back to just past the one record that begins at `from_tail`, dropping
/// every later record. Reads that record's body length from its header (a `u32` at [`RECORD_LEN_AT`]
/// in the 32-byte record header) to find its end — the records are unpadded, tail advancing by exactly
/// header + body — and writes the tail word back. `from_tail` is small in these tests, so the record
/// does not wrap the ring.
fn cut_log_after_one_record(segment: &mut AnchorSegment, partition: u16, from_tail: u64) {
  let capacity = segment.ring_words(RegionKind::Log(partition)).unwrap()[2].load(Ordering::Acquire);
  let data_at = slates_anchor::layout::RING_BYTES
    + usize::try_from(from_tail % capacity.max(1)).unwrap_or(0)
    + RECORD_LEN_AT;
  let body_len = {
    let mut word = [0u8; size_of::<u32>()];
    segment
      .region_read(RegionKind::Log(partition), data_at, &mut word)
      .unwrap();
    u64::from(u32::from_le_bytes(word))
  };
  let record_end =
    from_tail + u64::try_from(slates_db::record::RECORD_HEADER).unwrap_or(0) + body_len;
  segment.ring_words(RegionKind::Log(partition)).unwrap()[1].store(record_end, Ordering::Release);
}

/// Shape: the landed volume's name, and the bytes of a file the landing target held before the volume
/// overlaid it (the volume never wrote it).
const LANDED_VOLUME: &str = "landed";
const OUTSIDE_BYTES: &[u8] = b"on the target before the landing\n";

/// Mounts `name` on `daemon` over NFS: the stream and the root handle.
fn mounted_on(daemon: &Daemon, name: &str) -> (TcpStream, Vec<u8>) {
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon
    .mount_capability(name)
    .expect("the name's owner shard answers")
    .expect("the volume is served");
  let root = mount(&mut stream, &capability, 1);
  (stream, root)
}

/// §4.15 step 9, §4.8 (a landed scratch volume is an overlay of its target, durably): do: land a scratch
/// volume into a directory that already holds `outside`, read `outside` through the mount (the volume
/// overlays the target now), restart the daemon over the same segment and read it again; expect the same
/// bytes both times. Until 2026-09-30 the volume's record kept `Scratch`, so the restart rebuilt it without
/// the directory beneath it and `outside` was gone.
#[test]
fn a_landed_scratch_volume_keeps_its_base_across_a_restart() {
  let profile = common::machine_profile();
  let instance = format!("srv-landrestart-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("landrestart", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let target = common::target::target_dir();
  target.seed("outside", OUTSIDE_BYTES);
  let mut client = connect(&instance);
  let secret = first.segment().issuer_secret().unwrap();
  let volume = client.create(&scratch(LANDED_VOLUME)).unwrap();
  let (mut stream, root) = mounted_on(&first, LANDED_VOLUME);
  let file = create(&mut stream, &root, "f", 2);
  write(&mut stream, &file, BEFORE, 3);
  let grant = common::landing::approve(&mut client, &secret, volume, None, &target.path);
  match client.land(volume, None, &target.path, Filter::default(), Some(grant)) {
    Ok(slates_client::Landing::Landed(outcome)) => assert_eq!(outcome.state, "done"),
    other => panic!("the granted landing: {other:?}"),
  }
  let (mut stream, root) = mounted_on(&first, LANDED_VOLUME);
  let outside = lookup(&mut stream, &root, "outside", 4);
  assert_eq!(
    read(&mut stream, &outside, 5),
    OUTSIDE_BYTES,
    "the volume overlays the target"
  );
  drop(stream);
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let (mut stream, root) = mounted_on(&second, LANDED_VOLUME);
  let outside = lookup(&mut stream, &root, "outside", 6);
  assert_eq!(
    read(&mut stream, &outside, 7),
    OUTSIDE_BYTES,
    "after the restart the volume still overlays the directory it landed on"
  );
  let landed = lookup(&mut stream, &root, "f", 8);
  assert_eq!(read(&mut stream, &landed, 9), BEFORE);
  drop(client);
  second.stop();
}

/// §4.8, A-48 (an overlay's diverged state is imaged with its base, GAP-A9-6): do: land a scratch volume
/// onto a directory holding `outside` and `untouched`, so the volume overlays it; over NFS, overwrite
/// `outside` (a copy-up of a base file) and create `g`, each a `FILE_SYNC` write, and restart the daemon
/// over the same segment; expect both writes answered stable (an overlay the barrier could not image answers
/// `NFS3ERR_IO`, counted `BARRIER_UNCAPTURED`), and after the restart `outside` holding the copied-up bytes,
/// `g` its own, and `untouched` still read from the directory beneath.
#[test]
fn an_overlays_copied_up_and_created_files_survive_a_restart_over_their_base() {
  let profile = common::machine_profile();
  let instance = format!("srv-overlayrestart-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("overlayrestart", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let target = common::target::target_dir();
  target.seed("outside", OUTSIDE_BYTES);
  target.seed("untouched", BEFORE);
  let mut client = connect(&instance);
  let secret = first.segment().issuer_secret().unwrap();
  let volume = client.create(&scratch(LANDED_VOLUME)).unwrap();
  let grant = common::landing::approve(&mut client, &secret, volume, None, &target.path);
  match client.land(volume, None, &target.path, Filter::default(), Some(grant)) {
    Ok(slates_client::Landing::Landed(outcome)) => assert_eq!(outcome.state, "done"),
    other => panic!("the granted landing: {other:?}"),
  }
  let (mut stream, root) = mounted_on(&first, LANDED_VOLUME);
  let outside = lookup(&mut stream, &root, "outside", 2);
  write(&mut stream, &outside, AFTER, 3);
  let created = create(&mut stream, &root, "g", 4);
  write(&mut stream, &created, BEFORE, 5);
  drop(stream);
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let (mut stream, root) = mounted_on(&second, LANDED_VOLUME);
  let outside = lookup(&mut stream, &root, "outside", 6);
  assert_eq!(
    read(&mut stream, &outside, 7),
    AFTER,
    "the copied-up base file keeps the bytes written over it"
  );
  let created = lookup(&mut stream, &root, "g", 8);
  assert_eq!(read(&mut stream, &created, 9), BEFORE);
  let untouched = lookup(&mut stream, &root, "untouched", 10);
  assert_eq!(
    read(&mut stream, &untouched, 11),
    BEFORE,
    "a base file the overlay never wrote is still read from the directory beneath"
  );
  drop(client);
  second.stop();
}

/// §4.8, A-64 (a clone recovers beside its origin, sharing its origin snapshot's records). Do: write a file into a
/// volume over NFS, snapshot it, clone the snapshot, write the clone's own file, then restart the daemon over the
/// same segment. Expect: the recovered clone serves the inherited file's bytes and its own, and the origin its file.
/// A clone image recovered without its origin is refused, so serving both proves the clone was rebuilt over the
/// recovered origin.
#[test]
fn a_clone_recovers_over_its_recovered_origin_serving_inherited_and_its_own_files() {
  let profile = common::machine_profile();
  let instance = format!("srv-clonerec-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("clonerec", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let origin = client.create(&scratch("clone-origin")).unwrap();
  let (mut stream, root) = mounted_on(&first, "clone-origin");
  let inherited = create(&mut stream, &root, "inherited", 2);
  write(&mut stream, &inherited, BEFORE, 3);
  drop(stream);
  let snapshot = client.snapshot(origin).unwrap();
  // A clone lives on its origin's partition; its name must route there too to be mounted by name.
  let clone_name = clone_name_on_origin_partition("clone-origin", "the-clone");
  client
    .clone_snapshot(origin, snapshot, &clone_name)
    .unwrap();
  let (mut stream, root) = mounted_on(&first, &clone_name);
  let own = create(&mut stream, &root, "own", 4);
  write(&mut stream, &own, AFTER, 5);
  drop(stream);
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let (mut stream, root) = mounted_on(&second, &clone_name);
  let inherited = lookup(&mut stream, &root, "inherited", 6);
  assert_eq!(
    read(&mut stream, &inherited, 7),
    BEFORE,
    "the clone's inherited file"
  );
  let own = lookup(&mut stream, &root, "own", 8);
  assert_eq!(read(&mut stream, &own, 9), AFTER, "the clone's own file");
  drop(stream);
  let (mut stream, root) = mounted_on(&second, "clone-origin");
  let file = lookup(&mut stream, &root, "inherited", 10);
  assert_eq!(read(&mut stream, &file, 11), BEFORE, "the origin's file");
  drop(stream);
  drop(client);
  second.stop();
}

/// Shape: the power-of-two part of the reserve in [`a_volume_past_the_reserves_power_of_two_part_fills_and_survives_a_restart`].
const POWER_PART: u64 = 32 << 20;
/// Shape: the files that test writes, each [`FILL_FILE`] long: more than [`POWER_PART`] holds.
const FILL_FILES: u64 = 34;
/// Shape: one file of that test.
const FILL_FILE: usize = 1 << 20;

/// Reads `name` at the head, asking again while the client answers `Stalled` (the daemon alive but not yet answering:
/// the typed refusal a caller retries; a read is idempotent) for up to the recovery budget a restart is bounded by.
/// CI's TSan lane met it 3 runs of 3 on 2026-10-06: a read after the restart past the 1 s reply deadline, the runner's
/// four vCPUs shared by the suite's parallel tests under the sanitizer's slowdown; alone, or in the whole suite on a
/// 4-CPU container here, it never stalled (6 runs).
fn read_answered(
  client: &mut Client,
  volume: VolumeId,
  name: &str,
) -> Result<Vec<u8>, slates_client::ClientError> {
  let began = Instant::now();
  loop {
    match client.read(volume, name, slates_ipc::protocol::ReadAt::Head) {
      Err(slates_client::ClientError::Stalled { .. })
        if began.elapsed() < Duration::from_nanos(slates_db::replay::RECOVERY_BUDGET_NS) => {}
      answered => return answered,
    }
  }
}

/// §4.2 capacity (2026-10-05): a shard's reserve is RAM ÷ shards ÷ classes, rarely a power of two, and the buddy
/// arena used only its largest power-of-two part (128 MiB of a 170.7 MiB reserve under a 1 GiB cap, 25% stranded).
/// Do: give one shard a reserve of 1.5 × [`POWER_PART`], create a bounded volume larger than that power-of-two part,
/// write [`FILL_FILES`] files of a MiB into it over the anchor's content object, then restart the daemon. Expect the
/// volume admitted, every file written, and every byte read back after the restart (the blocks past the first region
/// are named in the image and claimed again). Before the fix the create was refused `BudgetExceeded`.
#[test]
fn a_volume_past_the_reserves_power_of_two_part_fills_and_survives_a_restart() {
  let profile = common::machine_profile();
  let instance = format!("srv-reserve-tail-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(1));
  config.reserve_per_shard = POWER_PART + POWER_PART / 2;
  let segment = anchor_segment("reserve-tail", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      size: SizeClass::Bounded {
        limit: POWER_PART + POWER_PART / 8,
      },
      ..scratch("tail")
    })
    .expect("a volume past the power-of-two part is admitted");
  let attachment = client
    .attach(volume, None, slates_ipc::protocol::Intent::Write)
    .unwrap()
    .attachment;
  for index in 0..FILL_FILES {
    let wrote = client.fs_write(
      (volume, attachment),
      &format!("f{index}"),
      &file_bytes(index),
      0o644,
    );
    assert_eq!(wrote.ok(), Some(FILL_FILE as u64), "file {index}");
  }
  first.stop();
  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  for index in 0..FILL_FILES {
    let read = read_answered(&mut client, volume, &format!("f{index}"));
    assert!(
      read.unwrap() == file_bytes(index),
      "file {index} after the restart"
    );
  }
  second.stop();
}

/// The bytes of file `index` in the fill tests: distinct per file, so a block read from the wrong place shows.
fn file_bytes(index: u64) -> Vec<u8> {
  (0..FILL_FILE)
    .map(|at| u8::try_from((at as u64 ^ index.wrapping_mul(0x9E37)) & 0xFF).unwrap())
    .collect()
}

/// Shape: one shard's slice in the pool tests: a power of two, so each slice is one extent.
const SLICE: u64 = 32 << 20;
/// Shape: shards in the pool tests.
const POOL_SHARDS: u16 = 2;

/// A name whose volume the partition `shard` owns, of a daemon with [`POOL_SHARDS`] partitions.
fn name_on(shard: u16, stem: &str) -> String {
  (0..)
    .map(|n| format!("{stem}-{n}"))
    .find(|name| slates_server::verbs::owner_of_name(name, usize::from(POOL_SHARDS)) == shard)
    .unwrap()
}

/// A-98 (§4.2): a volume is no longer capped by its shard's slice. Do: start two shards whose slices are [`SLICE`]
/// each, create a bounded volume of 1.25 × [`SLICE`] on shard 0, write 1.19 × [`SLICE`] of files into it, restart
/// the daemon over the same anchor, and read every file. Expect the volume admitted (its shard claims the other
/// slice's extent from the pool), every file written, and every byte back after the restart (the extent's owner
/// word and the image's blocks in it both survive). Before the pool, the create was refused `BudgetExceeded`.
#[test]
fn a_volume_larger_than_its_shards_slice_fills_from_the_pool_and_survives_a_restart() {
  let profile = common::machine_profile();
  let instance = format!("srv-pool-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(POOL_SHARDS));
  config.reserve_per_shard = SLICE;
  let segment = anchor_segment("pool", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      size: SizeClass::Bounded {
        limit: SLICE + SLICE / 4,
      },
      ..scratch(&name_on(0, "large"))
    })
    .expect("a volume larger than its shard's slice is admitted from the pool");
  let attachment = client
    .attach(volume, None, slates_ipc::protocol::Intent::Write)
    .unwrap()
    .attachment;
  let files = SLICE / u64::try_from(FILL_FILE).unwrap() * 19 / 16;
  for index in 0..files {
    let wrote = client.fs_write(
      (volume, attachment),
      &format!("f{index}"),
      &file_bytes(index),
      0o644,
    );
    assert_eq!(wrote.ok(), Some(FILL_FILE as u64), "file {index}");
  }
  first.stop();
  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  for index in 0..files {
    let read = client.read(
      volume,
      &format!("f{index}"),
      slates_ipc::protocol::ReadAt::Head,
    );
    assert!(
      read.unwrap() == file_bytes(index),
      "file {index} after the restart"
    );
  }
  second.stop();
}

/// A-98 (§4.2): shards meet in the pool. Do: on two shards of [`SLICE`] each, create a volume on shard 0 that takes
/// the other slice's extent too, try a volume on shard 1, then destroy the first and try again. Expect shard 1's
/// create refused `BudgetExceeded` while the pool is taken (typed, nothing created), and admitted once the destroy
/// completes and its shard's next publication returns the wholly free extents to the pool.
#[test]
fn a_shard_is_refused_while_another_holds_the_pool_and_admitted_once_it_returns() {
  let profile = common::machine_profile();
  let instance = format!("srv-pool-meet-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(POOL_SHARDS));
  config.reserve_per_shard = SLICE;
  let segment = anchor_segment("pool-meet", &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let bounded = |name: String, limit: u64| CreateSpec {
    size: SizeClass::Bounded { limit },
    ..scratch(&name)
  };
  let large = client
    .create(&bounded(name_on(0, "large"), SLICE + SLICE / 4))
    .expect("the pool covers it");
  let other = bounded(name_on(1, "other"), SLICE / 4);
  match client.create(&other) {
    Err(ClientError::Refused(slates_ipc::protocol::Refusal::BudgetExceeded { .. })) => {}
    refused => panic!("shard 1 must be refused while shard 0 holds the pool: {refused:?}"),
  }
  client.destroy(large).unwrap();
  let started = Instant::now();
  let admitted = loop {
    // The destroy completes in slices, and each mutating verb on shard 0 publishes; a create on shard 0 is one such
    // verb, so the extents return at the latest after it.
    let nudge = client.create(&bounded(name_on(0, "nudge"), 1 << 20));
    if let Ok(nudged) = nudge {
      client.destroy(nudged).unwrap();
    }
    match client.create(&other) {
      Ok(id) => break id,
      Err(_) if started.elapsed() < START_WAIT => std::hint::spin_loop(),
      Err(e) => panic!("shard 1 is admitted once the pool returns: {e}"),
    }
  };
  assert_ne!(admitted, large);
  daemon.stop();
}

/// Shape: the pool extent the crash test leaves claimed: partition 1's slice, its first (and, at a power-of-two
/// [`SLICE`], only) part (`POOL_EXTENTS_PER_PARTITION × 1 + 0`).
const OTHER_SLICE_EXTENT: usize = slates_anchor::layout::POOL_EXTENTS_PER_PARTITION;

/// A-98 crash between a claim and the first image that names it. A claim is one compare-and-swap of an owner word,
/// durable the moment it is taken; the image naming blocks in the extent is published after. Do: on two shards of
/// [`SLICE`], write files into a volume on shard 0 and stop; take the other slice's extent for partition 0 as a claim
/// made just before a kill would leave it; restart over the same anchor; create a volume on shard 0 (its first
/// publication) and then one on shard 1 needing half its own slice. Expect every file intact, the
/// orphaned extent free again after shard 0's first publication, and shard 1 admitted from it. Without the release
/// the extent would stay with partition 0 for the anchor's life and shard 1's create would be refused.
#[test]
fn an_extent_claimed_just_before_a_crash_is_returned_at_the_first_publication() {
  let profile = common::machine_profile();
  let instance = format!("srv-pool-crash-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(POOL_SHARDS));
  config.reserve_per_shard = SLICE;
  let segment = anchor_segment("pool-crash", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let bounded = |name: String, limit: u64| CreateSpec {
    size: SizeClass::Bounded { limit },
    ..scratch(&name)
  };
  let kept = client
    .create(&bounded(name_on(0, "kept"), SLICE / 8))
    .unwrap();
  let attachment = client
    .attach(kept, None, slates_ipc::protocol::Intent::Write)
    .unwrap()
    .attachment;
  let files = 3;
  for index in 0..files {
    let wrote = client.fs_write(
      (kept, attachment),
      &format!("f{index}"),
      &file_bytes(index),
      0o644,
    );
    assert_eq!(wrote.ok(), Some(FILL_FILE as u64), "file {index}");
  }
  first.stop();
  assert!(
    segment.pool_claim(OTHER_SLICE_EXTENT, 0).unwrap(),
    "the other slice's extent was free, and is now held as a crash after its claim leaves it"
  );
  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  for index in 0..files {
    let read = client.read(
      kept,
      &format!("f{index}"),
      slates_ipc::protocol::ReadAt::Head,
    );
    assert!(
      read.unwrap() == file_bytes(index),
      "file {index} after the restart"
    );
  }
  client
    .create(&bounded(name_on(0, "publish"), 1 << 20))
    .expect("shard 0 publishes");
  assert_eq!(
    segment.pool_owner(OTHER_SLICE_EXTENT).unwrap(),
    None,
    "the orphaned claim is returned at the first publication"
  );
  client
    .create(&bounded(name_on(1, "own"), SLICE / 2))
    .expect("shard 1 is admitted from its own slice again");
  second.stop();
}

/// The shards' total of status counter `kind`.
fn counter(client: &mut Client, kind: &str) -> u64 {
  client
    .daemon_status()
    .unwrap()
    .shards
    .iter()
    .flat_map(|shard| shard.refusals.iter())
    .filter(|refusal| refusal.kind == kind)
    .map(|refusal| refusal.count)
    .sum()
}

/// Shape: the files the sealing restart test writes.
const SEALED_FILES: u64 = 6;

/// A-99 (condition 9): do write files into a volume on a daemon whose node has a sealing root, then restart the
/// daemon over the same anchor and read every file. Expect chunks sealed in the arena (`content.sealed` moved, and no
/// seal fell back to the clear), and every byte read back after the restart: the new daemon draws a new salt, derives
/// the old life's keys from the identities the image records, and opens what the last daemon sealed.
#[test]
fn sealed_content_reads_back_byte_for_byte_after_a_restart() {
  let profile = common::machine_profile();
  let instance = format!("srv-sealed-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("sealed", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      size: SizeClass::Bounded { limit: 64 << 20 },
      ..scratch("sealed")
    })
    .unwrap();
  let attachment = client
    .attach(volume, None, slates_ipc::protocol::Intent::Write)
    .unwrap()
    .attachment;
  for index in 0..SEALED_FILES {
    let wrote = client.fs_write(
      (volume, attachment),
      &format!("f{index}"),
      &file_bytes(index),
      0o644,
    );
    assert_eq!(wrote.ok(), Some(FILL_FILE as u64), "file {index}");
  }
  assert!(
    counter(&mut client, "content.sealed") > 0,
    "chunks were sealed in the arena"
  );
  assert_eq!(
    counter(&mut client, "content.seal_refused"),
    0,
    "no seal fell back to the clear"
  );
  first.stop();
  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  for index in 0..SEALED_FILES {
    let read = client.read(
      volume,
      &format!("f{index}"),
      slates_ipc::protocol::ReadAt::Head,
    );
    assert!(
      read.unwrap() == file_bytes(index),
      "file {index} after the restart"
    );
  }
  second.stop();
}

/// Shape: how long the boot tests hold a shard's start: two of the control loop's liveness windows, so the shard's first
/// answer cannot arrive within one.
const HELD_START_NS: u64 = 2 * slates_server::daemon::LIVENESS_BUDGET_NS;

/// Boot (GAPS 2026-10-05): do start a daemon whose second shard spends two liveness windows on the CPU in its start, as
/// a long recovery does, then connect and create a volume; expect the daemon to serve, since the control loop's first
/// call to that shard waits while the shard is still working. Before the fix the control loop gave up after one window
/// and the daemon never served.
#[test]
fn a_shard_whose_start_outlasts_a_liveness_window_while_working_does_not_stop_the_daemon() {
  let profile = common::machine_profile();
  let instance = format!("srv-long-start-{}", std::process::id());
  let config =
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)).with_boot_fault(BootFault {
      partition: 1,
      kind: BootFaultKind::Busy,
      for_ns: HELD_START_NS,
    });
  let segment = anchor_segment("long-start", &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  client
    .create(&scratch("after-a-long-start"))
    .expect("a daemon whose shard started slowly serves");
  daemon.stop();
  drop(segment);
}

/// Boot (GAPS 2026-10-05): do start a daemon whose second shard sleeps through two liveness windows in its start, using
/// no CPU, as a stuck shard does; expect the daemon to stop beating within the client's start wait (so the anchor, which
/// restarts a daemon whose heartbeat lapses, restarts it) instead of beating on while it can never serve.
#[test]
fn a_shard_stuck_in_its_start_stops_the_heartbeat_so_the_anchor_restarts_the_daemon() {
  let profile = common::machine_profile();
  let instance = format!("srv-stuck-start-{}", std::process::id());
  let config =
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)).with_boot_fault(BootFault {
      partition: 1,
      kind: BootFaultKind::Stuck,
      for_ns: HELD_START_NS,
    });
  let segment = anchor_segment("stuck-start", &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let quiet = Duration::from_nanos(2 * slates_server::daemon::HEARTBEAT_NS);
  let started = Instant::now();
  let mut stopped = false;
  while started.elapsed() < START_WAIT {
    let before = segment.supervision().unwrap().heartbeat_ns();
    // The test plays the anchor, which watches the heartbeat from its own thread across time.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(quiet);
    let after = segment.supervision().unwrap().heartbeat_ns();
    if before > 0 && before == after {
      stopped = true;
      break;
    }
  }
  assert!(
    stopped,
    "the heartbeat stopped after the stuck shard was refused"
  );
  daemon.stop();
  drop(segment);
}

/// Boot (A-100): do start a daemon whose second shard spends two liveness windows on the CPU in its start, and stop the
/// daemon at once, while its control loop still waits on that shard; expect the stop to finish within the client's
/// start wait. (The heartbeat kept joinable until the boot was accepted hung `Daemon::stop` in
/// `an_acknowledged_replica_survives_a_warm_daemon_restart`, every run, and that test is its reproducer; this one passes
/// with either design and covers a stop during a long start.)
#[test]
fn a_daemon_stopped_while_its_boot_waits_on_a_shard_stops() {
  let profile = common::machine_profile();
  let instance = format!("srv-stop-mid-boot-{}", std::process::id());
  let config =
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)).with_boot_fault(BootFault {
      partition: 1,
      kind: BootFaultKind::Busy,
      for_ns: HELD_START_NS,
    });
  let segment = anchor_segment("stop-mid-boot", &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let (stopped, done) = std::sync::mpsc::channel();
  let stopper = std::thread::spawn(move || {
    daemon.stop();
    let _ = stopped.send(());
  });
  assert!(
    done.recv_timeout(START_WAIT).is_ok(),
    "the daemon stopped while its boot was waiting on a shard"
  );
  stopper.join().unwrap();
  drop(segment);
}

/// How many times `needle` occurs in the content memfd named `name` that this process holds. Only its data ranges are
/// read (`SEEK_DATA`/`SEEK_HOLE`, which tmpfs answers), so the multi-GiB object's never-written pages cost nothing and
/// are never allocated (`SparseObject::read` commits each page first, so the scan does not use it).
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // reading this process's own memfd through /proc, read-only
fn occurrences_in_content(name: &str, needle: &[u8]) -> usize {
  use std::os::unix::fs::FileExt;
  let Ok(fds) = std::fs::read_dir("/proc/self/fd") else {
    return 0;
  };
  for entry in fds.flatten() {
    let target = std::fs::read_link(entry.path()).unwrap_or_default();
    if !target.to_string_lossy().contains(&format!("memfd:{name}")) {
      continue;
    }
    let Ok(file) = std::fs::File::open(entry.path()) else {
      continue;
    };
    let mut count = 0;
    let mut at: u64 = 0;
    while let Ok(data) = rustix::fs::seek(&file, rustix::fs::SeekFrom::Data(at)) {
      let end = rustix::fs::seek(&file, rustix::fs::SeekFrom::Hole(data)).unwrap_or(data);
      let mut range = vec![0u8; usize::try_from(end - data).unwrap()];
      let read = file.read_at(&mut range, data).unwrap_or(0);
      count += range[..read]
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count();
      if end <= at {
        break;
      }
      at = end;
    }
    return count;
  }
  0
}

/// Shape: the reap ticks an idle shard is watched for further publications once its plaintext has gone.
#[cfg(target_os = "linux")]
const IDLE_TICKS_WATCHED: u64 = 3;

/// Condition 9, a deleted file, and the cost of the rule that scrubs it. Do: write a marker file, publish every shard (as
/// a transport's barrier does), delete the file, and leave the volume idle; then watch the shards' publications for a
/// few ticks. Expect: the marker gone from the content object within the bound, and then no further publication: the
/// reap tick publishes only while a free is deferred, so an idle shard costs nothing. The deletion's own scrub through a
/// kernel mount is proven, mutation-checked, by `a_deleted_files_plaintext_leaves_the_daemons_memory`
/// (`crates/cli/tests/cli.rs`); on this verb path the block is scrubbed with or without the rule.
#[cfg(target_os = "linux")]
#[test]
fn a_deleted_files_plaintext_leaves_the_content_object() {
  const MARKER: &[u8] = b"SLATES-DELETED-AT-REST-4c2d";
  let profile = common::machine_profile();
  let instance = format!("srv-rest-delete-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let tag = format!("rest-delete-{}", std::process::id());
  let segment = anchor_segment(&tag, &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client.create(&scratch("rest-delete")).unwrap();
  let attachment = client
    .attach(volume, None, slates_ipc::protocol::Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write((volume, attachment), "gone", &MARKER.repeat(32), 0o644)
    .unwrap();
  let content = format!("slates-con-{tag}");
  // Published while still open, as a FUSE `close` publishes it: the image names the file's plaintext block, so the
  // delete below can only defer its free.
  daemon.publish_every_shard().unwrap();
  let written = occurrences_in_content(&content, MARKER);
  client.fs_remove((volume, attachment), "gone").unwrap();
  let started = Instant::now();
  let mut left = occurrences_in_content(&content, MARKER);
  while left > 0 && started.elapsed() < START_WAIT {
    std::thread::yield_now();
    left = occurrences_in_content(&content, MARKER);
  }
  let published = |daemon: &Daemon| -> u64 {
    daemon
      .db_publication_counters()
      .map(|shards| shards.iter().map(|shard| shard.snapshots_taken).sum())
      .unwrap_or(0)
  };
  let before = published(&daemon);
  // A test may sleep (the lint's stated exception): the watch is wall time over the daemon's own reap ticks.
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_nanos(
    slates_server::daemon::LIVENESS_BUDGET_NS.saturating_mul(IDLE_TICKS_WATCHED),
  ));
  let after = published(&daemon);
  daemon.stop();
  drop(segment);
  assert!(
    written > 0,
    "the scan sees the plaintext right after the write (its control)"
  );
  assert_eq!(
    left, 0,
    "no copy of the deleted file's plaintext stays in the content object"
  );
  assert_eq!(
    after, before,
    "an idle shard with nothing deferred publishes nothing"
  );
}

/// Condition 9 (A-99: plaintext at rest only where it is being written), read as an attacker on the host reads it: the
/// anchor's content object itself. Do: write a small file carrying a marker; scan the content object; wait for the idle
/// sweep to seal it; scan again until the bound. Expect: the marker is there right after the write (the open extent,
/// plaintext by design, the scan's control) and gone after the seal. Before 2026-10-06 the seal moved the chunk and
/// deferred the old block's free to the next publication, and on an idle volume none came, so the plaintext stayed
/// (a live daemon's memfd: 400 copies after 25 s, `content.sealed` moved); the write log kept a second copy until a
/// later append overwrote it.
#[cfg(target_os = "linux")]
#[test]
fn a_sealed_files_plaintext_leaves_the_content_object() {
  const MARKER: &[u8] = b"SLATES-PLAINTEXT-AT-REST-1b9e";
  let profile = common::machine_profile();
  let instance = format!("srv-rest-scrub-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let tag = format!("rest-scrub-{}", std::process::id());
  let segment = anchor_segment(&tag, &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client.create(&scratch("rest-scrub")).unwrap();
  let attachment = client
    .attach(volume, None, slates_ipc::protocol::Intent::Write)
    .unwrap()
    .attachment;
  let bytes = MARKER.repeat(32);
  client
    .fs_write((volume, attachment), "secret", &bytes, 0o644)
    .unwrap();
  let content = format!("slates-con-{tag}");
  let written = occurrences_in_content(&content, MARKER);
  let started = Instant::now();
  while counter(&mut client, "content.sealed") == 0 && started.elapsed() < START_WAIT {
    std::hint::spin_loop();
  }
  let sealed = counter(&mut client, "content.sealed");
  let scrub_started = Instant::now();
  let mut left = occurrences_in_content(&content, MARKER);
  while left > 0 && scrub_started.elapsed() < START_WAIT {
    std::thread::yield_now();
    left = occurrences_in_content(&content, MARKER);
  }
  let read = client.read(volume, "secret", slates_ipc::protocol::ReadAt::Head);
  daemon.stop();
  drop(segment);
  assert!(
    written > 0,
    "the scan sees the open extent's plaintext right after the write (its control)"
  );
  assert!(sealed > 0, "the sweep sealed the idle file");
  assert_eq!(
    left, 0,
    "no copy of the sealed file's plaintext stays in the content object"
  );
  assert!(read.unwrap() == bytes, "the sealed file reads back whole");
}

/// A-99 (the idle sweep, live). Do: on a daemon with a sealing root, write one file smaller than a chunk (so no write
/// seals it) and then leave it. Expect: within the client's start wait the reap loop's sweep seals it (`content.sealed`
/// moves from zero with no refusal), and it reads back whole.
#[test]
fn an_idle_small_file_is_sealed_by_the_daemons_sweep() {
  let profile = common::machine_profile();
  let instance = format!("srv-idle-seal-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = anchor_segment("idle-seal", &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client.create(&scratch("idle-seal")).unwrap();
  let attachment = client
    .attach(volume, None, slates_ipc::protocol::Intent::Write)
    .unwrap()
    .attachment;
  let bytes: Vec<u8> = (0..1000u32)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  client
    .fs_write((volume, attachment), "small", &bytes, 0o644)
    .unwrap();
  assert_eq!(
    counter(&mut client, "content.sealed"),
    0,
    "a small write seals nothing itself"
  );
  let started = Instant::now();
  while counter(&mut client, "content.sealed") == 0 && started.elapsed() < START_WAIT {
    std::hint::spin_loop();
  }
  assert!(
    counter(&mut client, "content.sealed") > 0,
    "the sweep sealed the idle file"
  );
  assert_eq!(counter(&mut client, "content.seal_refused"), 0);
  let read = client.read(volume, "small", slates_ipc::protocol::ReadAt::Head);
  assert!(read.unwrap() == bytes, "the sealed file reads back");
  daemon.stop();
  drop(segment);
}
