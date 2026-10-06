//! The NFSv4.1/4.2 front end driven as a client drives it (A-35): `COMPOUND`s encoded on the wire,
//! served through `serve_compound` over a real `Export` of a scratch volume, the replies decoded and
//! checked against RFC 8881's rules — the session handshake, exactly-once replies from the slot cache,
//! open state and its refusals, and hostile compounds.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_bridge_nfs::Export;
use slates_bridge_nfs::v4::Nfsstat4;
use slates_bridge_nfs::v4::backend::serve_compound;
use slates_bridge_nfs::v4::compound::{COMPOUND_HEADER_BYTES, MIN_OPERATION_BYTES, Server, op};
use slates_bridge_nfs::v4::session::Limits;
use slates_bridge_nfs::v4::types::{Bitmap, ChannelAttrs, Stateid};
use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const VOLUME: VolumeId = VolumeId { bytes: [0x11; 16] };
const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;
/// `FATTR4_SIZE`, `FATTR4_MODE`, `FATTR4_FILEID` (RFC 7863).
const ATTR_SIZE: u32 = 4;
const ATTR_MODE: u32 = 33;
const ATTR_FILEID: u32 = 20;
/// `OPEN4_SHARE_ACCESS_*` / `OPEN4_SHARE_DENY_*`.
const READ: u32 = 1;
const WRITE: u32 = 2;
const BOTH: u32 = 3;
const DENY_NONE: u32 = 0;
/// `createmode4`.
const UNCHECKED: u32 = 0;
const GUARDED: u32 = 1;
/// `FILE_SYNC4`.
const FILE_SYNC: u32 = 2;

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

fn root_cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VOLUME,
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

fn export<'a>(bridge: &'a mut VolumeBridge<'_>, uid: u32) -> Export<'a> {
  Export::new(
    bridge,
    VOLUME,
    Principal::Uid { uid },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap()
}

thread_local! {
  /// The clock the tests serve compounds at, in nanoseconds; a test that measures a lease moves it.
  static NOW_NS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A server with the standalone offer, `max_clients` clients and a `lease_ns` lease.
fn server_with(max_clients: usize, lease_ns: u64) -> Server {
  let size = slates_bridge_nfs::procedures::MAX_TRANSFER + COMPOUND_HEADER_BYTES;
  Server::new(
    7,
    Limits {
      max_clients,
      max_sessions_per_client: 4,
      offer: ChannelAttrs {
        header_pad: 0,
        max_request: size,
        max_response: size,
        max_response_cached: size,
        max_operations: size / MIN_OPERATION_BYTES,
        max_requests: 4,
      },
      lease_ns,
    },
  )
}

/// One `COMPOUND`'s decoded reply: the overall status and each operation's (opnum, status, body).
struct Reply {
  status: u32,
  results: Vec<(u32, u32)>,
  bytes: Vec<u8>,
}

impl Reply {
  /// The results after the frame, owned (for a reply whose frame is not kept).
  fn walk_owned(&self) -> Vec<u8> {
    self.walk().rest().to_vec()
  }

  /// The body reader positioned after the frame, for walking the results in order.
  fn walk(&self) -> XdrReader<'_> {
    let mut reader = XdrReader::new(&self.bytes);
    reader.u32().unwrap();
    reader.opaque(1024).unwrap();
    reader.u32().unwrap();
    reader
  }
}

/// Decodes the frame and the (opnum, status) of each result; the bodies are walked by the test.
fn decode_frame(bytes: Vec<u8>) -> Reply {
  let mut reader = XdrReader::new(&bytes);
  let status = reader.u32().unwrap();
  reader.opaque(1024).unwrap();
  let count = reader.u32().unwrap();
  // Only the statuses are taken here: a result body's length depends on its operation, so the last
  // result's opnum and status are recovered from the tail of an error reply, and success replies are
  // walked by the caller.
  let mut results = Vec::new();
  if count > 0 && status != Nfsstat4::Ok.wire() {
    let tail = &bytes[bytes.len() - 8..];
    let mut tail = XdrReader::new(tail);
    results.push((tail.u32().unwrap(), tail.u32().unwrap()));
  }
  Reply {
    status,
    results,
    bytes,
  }
}

/// Expects the next result to be `opnum` succeeding.
fn expect_ok(reader: &mut XdrReader<'_>, opnum: u32) {
  assert_eq!(reader.u32().unwrap(), opnum, "result order");
  assert_eq!(
    reader.u32().unwrap(),
    Nfsstat4::Ok.wire(),
    "op {opnum} status"
  );
}

/// A test client: builds compounds, tracks its session's slot 0 sequence.
struct Client {
  owner: Vec<u8>,
  clientid: u64,
  sessionid: [u8; 16],
  sequence: u32,
  /// The minor version its compounds carry.
  minor: u32,
}

impl Client {
  fn call(service: &mut Export<'_>, server: &mut Server, args: &[u8]) -> Reply {
    decode_frame(
      serve_compound(
        service,
        server,
        0,
        NOW_NS.with(std::cell::Cell::get),
        (args, args.len()),
      )
      .expect("the compound completes in one poll"),
    )
  }

  /// EXCHANGE_ID then CREATE_SESSION for `owner`.
  fn connect(service: &mut Export<'_>, server: &mut Server, owner: &[u8]) -> Client {
    let mut args = frame(1, 1);
    args.u32(op::EXCHANGE_ID);
    args.fixed(&[1; 8]);
    args.opaque(owner);
    args.u32(0); // flags
    args.u32(0); // SP4_NONE
    args.u32(0); // no implementation id
    let reply = Client::call(service, server, args.as_slice());
    assert_eq!(reply.status, Nfsstat4::Ok.wire(), "EXCHANGE_ID");
    let mut body = reply.walk();
    expect_ok(&mut body, op::EXCHANGE_ID);
    let clientid = body.u64().unwrap();
    let sequenceid = body.u32().unwrap();

    let mut args = frame(1, 1);
    args.u32(op::CREATE_SESSION);
    args.u64(clientid);
    args.u32(sequenceid);
    args.u32(0);
    let asked = ChannelAttrs {
      header_pad: 0,
      max_request: 1 << 20,
      max_response: 1 << 20,
      max_response_cached: 1 << 20,
      max_operations: 16,
      max_requests: 8,
    };
    asked.encode(&mut args);
    asked.encode(&mut args);
    args.u32(0x4000_0000); // callback program
    args.u32(1);
    args.u32(0); // AUTH_NONE
    let reply = Client::call(service, server, args.as_slice());
    assert_eq!(reply.status, Nfsstat4::Ok.wire(), "CREATE_SESSION");
    let mut body = reply.walk();
    expect_ok(&mut body, op::CREATE_SESSION);
    let mut sessionid = [0u8; 16];
    sessionid.copy_from_slice(body.fixed(16).unwrap());
    Client {
      owner: owner.to_vec(),
      clientid,
      sessionid,
      sequence: 0,
      minor: 1,
    }
  }

  /// A compound of `count` operations after a SEQUENCE on slot 0 with the next sequence id.
  fn sequenced(&mut self, count: u32) -> XdrWriter {
    self.sequence += 1;
    self.sequenced_at(self.sequence, count)
  }

  /// A compound whose SEQUENCE carries `sequence` on slot 0 (a retry reuses the last one).
  fn sequenced_at(&self, sequence: u32, count: u32) -> XdrWriter {
    let mut args = frame(self.minor, count + 1);
    args.u32(op::SEQUENCE);
    args.fixed(&self.sessionid);
    args.u32(sequence);
    args.u32(0); // slot
    args.u32(0); // highest slot
    args.bool(true);
    args
  }

  /// OPEN `name` in the current directory for this client's owner (or `owner`), creating it with
  /// `create` = (createmode, mode, size) when given.
  fn open_args(
    &self,
    args: &mut XdrWriter,
    owner: &[u8],
    name: &str,
    access: u32,
    deny: u32,
    create: Option<(u32, Option<u32>, Option<u64>)>,
  ) {
    args.u32(op::OPEN);
    args.u32(0);
    args.u32(access);
    args.u32(deny);
    args.u64(self.clientid);
    args.opaque(owner);
    match create {
      Some((mode_kind, mode, size)) => {
        args.u32(1); // OPEN4_CREATE
        args.u32(mode_kind);
        let mut bits = Vec::new();
        let mut values = XdrWriter::new();
        if let Some(size) = size {
          bits.push(ATTR_SIZE);
          values.u64(size);
        }
        if let Some(mode) = mode {
          bits.push(ATTR_MODE);
          values.u32(mode);
        }
        Bitmap::of(&bits).encode(args);
        args.opaque(values.as_slice());
      }
      None => args.u32(0), // OPEN4_NOCREATE
    }
    args.u32(0); // CLAIM_NULL
    args.opaque(name.as_bytes());
  }
}

/// A COMPOUND's frame: an empty tag, the minor version and the operation count.
fn frame(minor: u32, count: u32) -> XdrWriter {
  let mut args = XdrWriter::new();
  args.opaque(b"");
  args.u32(minor);
  args.u32(count);
  args
}

/// Skips a SEQUENCE result body.
fn skip_sequence(body: &mut XdrReader<'_>) {
  expect_ok(body, op::SEQUENCE);
  body.fixed(16 + 5 * 4).unwrap();
}

/// Reads an OPEN result body: the state id; the change info, flags, attrset and delegation skipped.
fn open_result(body: &mut XdrReader<'_>) -> (Stateid, Bitmap) {
  expect_ok(body, op::OPEN);
  let stateid = Stateid::decode(body).unwrap();
  body.fixed(4 + 8 + 8 + 4).unwrap();
  let attrset = Bitmap::decode(body).unwrap();
  body.u32().unwrap();
  (stateid, attrset)
}

/// Opens (creating if absent, unchecked) `name` at the root for `owner`: the state id and file handle.
fn open_at_root(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  owner: &[u8],
  name: &str,
  access: u32,
  deny: u32,
) -> Result<(Stateid, Vec<u8>), u32> {
  let mut args = client.sequenced(3);
  args.u32(op::PUTROOTFH);
  client.open_args(
    &mut args,
    owner,
    name,
    access,
    deny,
    Some((UNCHECKED, Some(0o644), None)),
  );
  args.u32(op::GETFH);
  let reply = Client::call(service, server, args.as_slice());
  if reply.status != Nfsstat4::Ok.wire() {
    return Err(reply.status);
  }
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  let (stateid, _) = open_result(&mut body);
  expect_ok(&mut body, op::GETFH);
  Ok((stateid, body.opaque(128).unwrap().to_vec()))
}

/// READ `count` bytes at 0 of `fh` under `stateid`: the data, or the refusal status.
fn read(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  fh: &[u8],
  stateid: Stateid,
) -> Result<Vec<u8>, u32> {
  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(fh);
  args.u32(op::READ);
  stateid.encode(&mut args);
  args.u64(0);
  args.u32(4096);
  let reply = Client::call(service, server, args.as_slice());
  if reply.status != Nfsstat4::Ok.wire() {
    return Err(reply.status);
  }
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTFH);
  expect_ok(&mut body, op::READ);
  let _eof = body.bool().unwrap();
  Ok(body.opaque(4096).unwrap().to_vec())
}

/// READDIR of the root asking size, mode and file id: each entry's (name, size), checking the reserved
/// cookies and the attributes returned, and that the listing is complete.
fn list_root(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
) -> Vec<(String, u64)> {
  let mut args = client.sequenced(2);
  args.u32(op::PUTROOTFH);
  args.u32(op::READDIR);
  args.u64(0);
  args.fixed(&[0; 8]);
  args.u32(4096);
  args.u32(8192);
  Bitmap::of(&[ATTR_SIZE, ATTR_MODE, ATTR_FILEID]).encode(&mut args);
  let reply = Client::call(service, server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire(), "READDIR");
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  expect_ok(&mut body, op::READDIR);
  body.fixed(8).unwrap();
  let mut names = Vec::new();
  while body.bool().unwrap() {
    let cookie = body.u64().unwrap();
    assert!(
      cookie > 2,
      "cookies 0, 1 and 2 are reserved (RFC 8881 §18.23.3)"
    );
    let name = body.string(255).unwrap().to_owned();
    let bitmap = Bitmap::decode(&mut body).unwrap();
    assert!(bitmap.has(ATTR_SIZE) && bitmap.has(ATTR_MODE));
    let values = body.opaque(1024).unwrap();
    let mut values = XdrReader::new(values);
    let size = values.u64().unwrap();
    names.push((name, size));
  }
  assert!(body.bool().unwrap(), "the listing is complete");
  names
}

/// A client opens a new file, writes it, reads it back, closes it, and finds it in a directory listing
/// with the size it wrote — the walk a Linux `vers=4.1` mount makes for `echo > f; cat f; ls -l`.
/// A-35; RFC 8881 §18.16, §18.32, §18.22, §18.2, §18.23.
#[test]
fn a_client_opens_writes_reads_closes_and_lists_a_file() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");

  let mut args = client.sequenced(3);
  args.u32(op::PUTROOTFH);
  let owner = client.owner.clone();
  client.open_args(
    &mut args,
    &owner,
    "notes.txt",
    BOTH,
    DENY_NONE,
    Some((GUARDED, Some(0o640), None)),
  );
  args.u32(op::GETFH);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire());
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  let (stateid, attrset) = open_result(&mut body);
  assert!(attrset.has(ATTR_MODE), "the create reports the mode it set");
  expect_ok(&mut body, op::GETFH);
  let fh = body.opaque(128).unwrap().to_vec();

  let payload = b"written through NFSv4.1";
  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(&fh);
  args.u32(op::WRITE);
  stateid.encode(&mut args);
  args.u64(0);
  args.u32(FILE_SYNC);
  args.opaque(payload);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire(), "WRITE");
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTFH);
  expect_ok(&mut body, op::WRITE);
  assert_eq!(
    body.u32().unwrap() as usize,
    payload.len(),
    "every byte written"
  );

  assert_eq!(
    read(&mut client, &mut service, &mut server, &fh, stateid).unwrap(),
    payload
  );

  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(&fh);
  args.u32(op::CLOSE);
  args.u32(0);
  stateid.encode(&mut args);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire(), "CLOSE");
  assert_eq!(
    service.file_states().unwrap().open_count(),
    0,
    "the close released the open at the owner"
  );

  let names = list_root(&mut client, &mut service, &mut server);
  assert_eq!(
    names,
    vec![("notes.txt".to_owned(), payload.len() as u64)],
    "no dot entries, the size written"
  );
}

/// A request retried on its slot with the same sequence id is answered with the kept reply, byte for
/// byte, and not run again: a retried CREATE does not become `NFS4ERR_EXIST`, while a new request that
/// creates the same name does. A-35; RFC 8881 §2.10.6.1.
#[test]
fn a_retried_request_gets_the_kept_reply_and_is_not_run_again() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");

  let mkdir = |args: &mut XdrWriter| {
    args.u32(op::PUTROOTFH);
    args.u32(op::CREATE);
    args.u32(2); // NF4DIR
    args.opaque(b"build");
    Bitmap::default().encode(args);
    args.opaque(b"");
  };
  let mut args = client.sequenced(2);
  mkdir(&mut args);
  let first = serve_compound(
    &mut service,
    &mut server,
    0,
    NOW_NS.with(std::cell::Cell::get),
    (args.as_slice(), args.len()),
  )
  .unwrap();
  assert_eq!(
    decode_frame(first.clone()).status,
    Nfsstat4::Ok.wire(),
    "the first CREATE"
  );

  let mut retry = client.sequenced_at(client.sequence, 2);
  mkdir(&mut retry);
  let replayed = serve_compound(
    &mut service,
    &mut server,
    0,
    NOW_NS.with(std::cell::Cell::get),
    (retry.as_slice(), retry.len()),
  )
  .unwrap();
  assert_eq!(replayed, first, "the retry is answered from the slot cache");

  let mut again = client.sequenced(2);
  mkdir(&mut again);
  let reply = Client::call(&mut service, &mut server, again.as_slice());
  assert_eq!(
    reply.status,
    Nfsstat4::Exist.wire(),
    "a new request runs, and the name exists"
  );
  assert_eq!(reply.results, vec![(op::CREATE, Nfsstat4::Exist.wire())]);
}

/// Outside a session only the session operations are served, each alone; a minor version this server
/// does not speak is refused; an unknown operation number inside a session is `OP_ILLEGAL`, reported
/// under `OP_ILLEGAL`. A-35; RFC 8881 §15.1, §16.2.3, §18.46.3.
#[test]
fn outside_a_session_only_the_session_operations_are_served() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();

  let mut args = frame(1, 1);
  args.u32(op::PUTROOTFH);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::OpNotInSession.wire());

  let mut args = frame(1, 2);
  args.u32(op::EXCHANGE_ID);
  args.fixed(&[1; 8]);
  args.opaque(b"host-a");
  args.u32(0);
  args.u32(0);
  args.u32(0);
  args.u32(op::PUTROOTFH);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::NotOnlyOp.wire());

  for minor in [0, 3] {
    let mut args = frame(minor, 0);
    args.u32(op::PUTROOTFH);
    let reply = Client::call(&mut service, &mut server, args.as_slice());
    assert_eq!(
      reply.status,
      Nfsstat4::MinorVersMismatch.wire(),
      "minor {minor}"
    );
  }

  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let mut args = client.sequenced(1);
  args.u32(9999);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::OpIllegal.wire());
  assert_eq!(
    reply.results,
    vec![(op::ILLEGAL, Nfsstat4::OpIllegal.wire())]
  );
}

/// An open-owner's second open of a file shares its state id, advanced; after the close, that state id
/// serves nothing, and an earlier seqid of a live open is `NFS4ERR_OLD_STATEID`. A-35; RFC 8881
/// §9.1.4.1, §8.2.2.
#[test]
fn a_reopen_by_the_same_owner_shares_one_state_id_and_close_releases_it() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let owner = client.owner.clone();

  let (first, fh) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "log",
    READ,
    DENY_NONE,
  )
  .unwrap();
  let (second, _) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "log",
    BOTH,
    DENY_NONE,
  )
  .unwrap();
  assert_eq!(second.other, first.other, "one state id per owner and file");
  assert_eq!(second.seqid, first.seqid + 1, "the upgrade advances it");
  assert_eq!(service.file_states().unwrap().open_count(), 1);

  assert_eq!(
    read(&mut client, &mut service, &mut server, &fh, first),
    Err(Nfsstat4::OldStateid.wire())
  );
  assert!(read(&mut client, &mut service, &mut server, &fh, second).is_ok());

  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(&fh);
  args.u32(op::CLOSE);
  args.u32(0);
  second.encode(&mut args);
  assert_eq!(
    Client::call(&mut service, &mut server, args.as_slice()).status,
    Nfsstat4::Ok.wire()
  );
  assert_eq!(service.file_states().unwrap().open_count(), 0);
  assert_eq!(
    read(&mut client, &mut service, &mut server, &fh, second),
    Err(Nfsstat4::BadStateid.wire())
  );
}

/// A state id serves only the file it opened: presenting one file's open for another is
/// `NFS4ERR_BAD_STATEID`. A-35; RFC 8881 §8.2.4.
#[test]
fn a_state_id_serves_only_its_own_file() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let owner = client.owner.clone();
  let (for_a, _) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "a",
    READ,
    DENY_NONE,
  )
  .unwrap();
  let (_, fh_b) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "b",
    READ,
    DENY_NONE,
  )
  .unwrap();
  assert_eq!(
    read(&mut client, &mut service, &mut server, &fh_b, for_a),
    Err(Nfsstat4::BadStateid.wire())
  );
}

/// Opening an existing file checks the caller's permission for the access asked: a read-only file of
/// another owner opens for reading and is `NFS4ERR_ACCESS` for writing. A-35; RFC 8881 §18.16.3.
#[test]
fn an_open_of_an_existing_file_checks_the_callers_permission() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let cx = root_cx();
  let root = bridge.root(&cx).unwrap();
  bridge
    .create(ObjectId::new(root, 0), &cx, "config", 0o644, 0)
    .unwrap();
  let mut service = export(&mut bridge, 1000);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let owner = client.owner.clone();

  let open = |client: &mut Client, service: &mut Export<'_>, server: &mut Server, access| {
    let mut args = client.sequenced(2);
    args.u32(op::PUTROOTFH);
    client.open_args(&mut args, &owner, "config", access, DENY_NONE, None);
    Client::call(service, server, args.as_slice()).status
  };
  assert_eq!(
    open(&mut client, &mut service, &mut server, READ),
    Nfsstat4::Ok.wire()
  );
  assert_eq!(
    open(&mut client, &mut service, &mut server, WRITE),
    Nfsstat4::Access.wire()
  );
}

/// A share that denies writing refuses another owner's open for writing (`NFS4ERR_SHARE_DENIED`) and
/// admits its open for reading. A-35; RFC 8881 §9.7.
#[test]
fn a_conflicting_share_is_denied() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  open_at_root(
    &mut client,
    &mut service,
    &mut server,
    b"first",
    "shared",
    READ,
    WRITE,
  )
  .unwrap();
  assert_eq!(
    open_at_root(
      &mut client,
      &mut service,
      &mut server,
      b"second",
      "shared",
      WRITE,
      DENY_NONE
    )
    .err(),
    Some(Nfsstat4::ShareDenied.wire())
  );
  assert!(
    open_at_root(
      &mut client,
      &mut service,
      &mut server,
      b"second",
      "shared",
      READ,
      DENY_NONE
    )
    .is_ok()
  );
}

/// An `UNCHECKED` open of a file that exists opens it and applies only the size asked (the truncate of
/// `O_TRUNC`), leaving its mode. A-35; RFC 8881 §18.16.3.
#[test]
fn an_unchecked_open_of_an_existing_file_truncates_it_and_keeps_its_mode() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let cx = root_cx();
  let root = bridge.root(&cx).unwrap();
  let file = bridge
    .create(ObjectId::new(root, 0), &cx, "out", 0o600, 0)
    .unwrap();
  bridge
    .write(
      ObjectId::new(file.0.ino, file.0.generation),
      &cx,
      0,
      b"old contents",
    )
    .unwrap();
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let owner = client.owner.clone();

  let mut args = client.sequenced(3);
  args.u32(op::PUTROOTFH);
  client.open_args(
    &mut args,
    &owner,
    "out",
    WRITE,
    DENY_NONE,
    Some((UNCHECKED, Some(0o777), Some(0))),
  );
  args.u32(op::GETATTR);
  Bitmap::of(&[ATTR_SIZE, ATTR_MODE]).encode(&mut args);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire());
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  let (_, attrset) = open_result(&mut body);
  assert!(
    attrset.has(ATTR_SIZE) && !attrset.has(ATTR_MODE),
    "only the size was set"
  );
  expect_ok(&mut body, op::GETATTR);
  Bitmap::decode(&mut body).unwrap();
  let values = body.opaque(64).unwrap();
  let mut values = XdrReader::new(values);
  assert_eq!(values.u64().unwrap(), 0, "truncated");
  assert_eq!(values.u32().unwrap() & 0o7777, 0o600, "the mode kept");
}

/// A full client table makes room from the clients whose lease has lapsed, and their opens go with
/// them; while every client's lease is live, a new client is `NFS4ERR_DELAY`. A-35; RFC 8881
/// §8.3, §18.35.4.
#[test]
fn a_lapsed_client_makes_room_and_its_opens_go_with_it() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);

  let mut live = server_with(1, u64::MAX);
  Client::connect(&mut service, &mut live, b"host-a");
  let mut args = frame(1, 1);
  args.u32(op::EXCHANGE_ID);
  args.fixed(&[1; 8]);
  args.opaque(b"host-b");
  args.u32(0);
  args.u32(0);
  args.u32(0);
  assert_eq!(
    Client::call(&mut service, &mut live, args.as_slice()).status,
    Nfsstat4::Delay.wire()
  );

  const LEASE_NS: u64 = 1_000;
  let mut lapsing = server_with(1, LEASE_NS);
  let mut first = Client::connect(&mut service, &mut lapsing, b"host-a");
  let owner = first.owner.clone();
  open_at_root(
    &mut first,
    &mut service,
    &mut lapsing,
    &owner,
    "held",
    READ,
    DENY_NONE,
  )
  .unwrap();
  assert_eq!(service.file_states().unwrap().open_count(), 1);
  // At exactly one lease since its last request the client still holds its place.
  NOW_NS.with(|now| now.set(now.get() + LEASE_NS));
  let mut refused = frame(1, 1);
  refused.u32(op::EXCHANGE_ID);
  refused.fixed(&[1; 8]);
  refused.opaque(b"host-b");
  refused.u32(0);
  refused.u32(0);
  refused.u32(0);
  assert_eq!(
    Client::call(&mut service, &mut lapsing, refused.as_slice()).status,
    Nfsstat4::Delay.wire(),
    "a lease is not lapsed until it has passed"
  );
  NOW_NS.with(|now| now.set(now.get() + 1));
  Client::connect(&mut service, &mut lapsing, b"host-b");
  assert_eq!(
    lapsing.sessions.client_count(),
    1,
    "the lapsed client made room"
  );
  assert_eq!(
    service.file_states().unwrap().open_count(),
    0,
    "and its open went with it, at the owner"
  );
}

/// Hostile compounds are refused with a status, never a panic: truncated arguments, a length word of
/// `u32::MAX`, more operations than the channel carries, and a name with a separator. A-35.
#[test]
fn hostile_compounds_are_refused_with_a_status() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();

  let truncated = [0u8, 0, 0];
  assert_eq!(
    Client::call(&mut service, &mut server, &truncated).status,
    Nfsstat4::Badxdr.wire()
  );

  let mut huge_tag = XdrWriter::new();
  huge_tag.u32(u32::MAX);
  assert_eq!(
    Client::call(&mut service, &mut server, huge_tag.as_slice()).status,
    Nfsstat4::Badxdr.wire()
  );

  let too_many = frame(1, u32::MAX);
  assert_eq!(
    Client::call(&mut service, &mut server, too_many.as_slice()).status,
    Nfsstat4::TooManyOps.wire()
  );

  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let mut args = client.sequenced(2);
  args.u32(op::PUTROOTFH);
  args.u32(op::LOOKUP);
  args.opaque(b"a/b");
  assert_eq!(
    Client::call(&mut service, &mut server, args.as_slice()).status,
    Nfsstat4::Badname.wire()
  );

  let mut args = client.sequenced(2);
  args.u32(op::PUTROOTFH);
  args.u32(op::LOOKUP);
  args.u32(u32::MAX);
  assert_eq!(
    Client::call(&mut service, &mut server, args.as_slice()).status,
    Nfsstat4::Badxdr.wire()
  );

  let mut args = client.sequenced(1);
  args.u32(op::GETFH);
  assert_eq!(
    Client::call(&mut service, &mut server, args.as_slice()).status,
    Nfsstat4::Nofilehandle.wire()
  );
}

/// Sends one ONC RPC call (AUTH_NONE) for `program` `version` over `stream`: the reply's accept status
/// and what follows it.
fn rpc(
  stream: &mut std::net::TcpStream,
  program: u32,
  version: u32,
  procedure: u32,
  args: &[u8],
) -> (u32, Vec<u8>) {
  use std::io::{Read, Write};
  let mut body = XdrWriter::new();
  for field in [1, 0, 2, program, version, procedure, 0, 0, 0, 0] {
    body.u32(field); // xid, CALL, RPC version 2, the call, AUTH_NONE credential and verifier
  }
  body.fixed(args);
  let marker = 0x8000_0000u32 | u32::try_from(body.len()).unwrap();
  stream.write_all(&marker.to_be_bytes()).unwrap();
  stream.write_all(body.as_slice()).unwrap();
  let mut marker = [0u8; 4];
  stream.read_exact(&mut marker).unwrap();
  let mut reply = vec![0u8; (u32::from_be_bytes(marker) & 0x7fff_ffff) as usize];
  stream.read_exact(&mut reply).unwrap();
  let mut reader = XdrReader::new(&reply);
  reader.fixed(12).unwrap(); // xid, REPLY, MSG_ACCEPTED
  reader.u32().unwrap();
  reader.opaque(400).unwrap(); // the verifier
  let accept = reader.u32().unwrap();
  (accept, reader.rest().to_vec())
}

/// Over a real socket, version 4 of the NFS program is served beside version 3 and any other version
/// is `PROG_MISMATCH` naming 3–4; a v3 procedure outside RFC 1813's (the v4 front end's extensions) is
/// `PROC_UNAVAIL`; a session made on one connection carries on over the next, since the
/// listener, not the connection, holds the v4 state (RFC 8881 §2.10.3: a client reconnects and
/// continues its session). A-35.
#[test]
fn a_session_outlives_its_connection_and_other_versions_are_mismatched() {
  use std::net::{TcpListener, TcpStream};
  /// `PROG_MISMATCH` and `SUCCESS` accept statuses; the NFS program number (RFC 5531, RFC 1813).
  const SUCCESS: u32 = 0;
  const PROG_MISMATCH: u32 = 2;
  const NFS_PROGRAM: u32 = 100_003;
  const CONNECTIONS: usize = 2;
  let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
  let port = listener.local_addr().unwrap().port();
  let serving = std::thread::spawn(move || {
    let mut store = store();
    let mut vol = volume(&mut store);
    let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
    let mut service = export(&mut bridge, 0);
    let mut v4 = Server::standalone();
    for _ in 0..CONNECTIONS {
      let (mut stream, _) = listener.accept().unwrap();
      slates_bridge_nfs::serve_connection(&mut stream, &mut service, &mut v4, port);
    }
  });

  let mut first = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let (accept, range) = rpc(&mut first, NFS_PROGRAM, 2, 0, &[]);
  assert_eq!(accept, PROG_MISMATCH);
  let mut range = XdrReader::new(&range);
  assert_eq!((range.u32().unwrap(), range.u32().unwrap()), (3, 4));
  assert_eq!(
    rpc(&mut first, NFS_PROGRAM, 4, 0, &[]).0,
    SUCCESS,
    "v4 NULL"
  );
  assert_eq!(
    rpc(&mut first, NFS_PROGRAM, 3, 0, &[]).0,
    SUCCESS,
    "v3 NULL"
  );
  /// Format: `PROC_UNAVAIL` (RFC 5531).
  const PROC_UNAVAIL: u32 = 3;
  assert_eq!(
    rpc(
      &mut first,
      NFS_PROGRAM,
      3,
      slates_bridge_nfs::procedures::extension::SEEK,
      &[]
    )
    .0,
    PROC_UNAVAIL,
    "the v4 front end's extension procedures are not served from the wire"
  );

  let mut args = frame(2, 1);
  args.u32(op::EXCHANGE_ID);
  args.fixed(&[1; 8]);
  args.opaque(b"host-a");
  args.u32(0);
  args.u32(0);
  args.u32(0);
  let (_, reply) = rpc(&mut first, NFS_PROGRAM, 4, 1, args.as_slice());
  let body = decode_frame(reply).walk_owned();
  let mut body = XdrReader::new(&body);
  expect_ok(&mut body, op::EXCHANGE_ID);
  let clientid = body.u64().unwrap();
  let sequenceid = body.u32().unwrap();
  let mut args = frame(2, 1);
  args.u32(op::CREATE_SESSION);
  args.u64(clientid);
  args.u32(sequenceid);
  args.u32(0);
  let asked = ChannelAttrs {
    max_request: 1 << 20,
    max_response: 1 << 20,
    max_response_cached: 1 << 20,
    max_operations: 16,
    max_requests: 1,
    ..ChannelAttrs::default()
  };
  asked.encode(&mut args);
  asked.encode(&mut args);
  args.u32(0);
  args.u32(0);
  let (_, reply) = rpc(&mut first, NFS_PROGRAM, 4, 1, args.as_slice());
  let body = decode_frame(reply).walk_owned();
  let mut body = XdrReader::new(&body);
  expect_ok(&mut body, op::CREATE_SESSION);
  let mut sessionid = [0u8; 16];
  sessionid.copy_from_slice(body.fixed(16).unwrap());
  drop(first);

  let mut second = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let client = Client {
    owner: b"host-a".to_vec(),
    clientid,
    sessionid,
    sequence: 0,
    minor: 2,
  };
  let mut args = client.sequenced_at(1, 1);
  args.u32(op::PUTROOTFH);
  let (accept, reply) = rpc(&mut second, NFS_PROGRAM, 4, 1, args.as_slice());
  assert_eq!(accept, SUCCESS);
  assert_eq!(
    decode_frame(reply).status,
    Nfsstat4::Ok.wire(),
    "the session made on the first connection serves the second"
  );
  drop(second);
  serving.join().unwrap();
}

/// `nfs_lock_type4` values and the lock operations' result bodies (RFC 7863).
const READ_LT: u32 = 1;
const WRITE_LT: u32 = 2;

/// Appends a LOCK for `owner` under the open `open` (a lock-owner new to it), or under the lock state
/// `existing`.
fn lock_args(
  args: &mut XdrWriter,
  kind: u32,
  offset: u64,
  length: u64,
  open: Stateid,
  existing: Option<Stateid>,
  owner: &[u8],
) {
  args.u32(op::LOCK);
  args.u32(kind);
  args.bool(false); // reclaim
  args.u64(offset);
  args.u64(length);
  match existing {
    None => {
      args.bool(true);
      args.u32(0);
      open.encode(args);
      args.u32(0);
      args.u64(0); // ignored: the session names the client
      args.opaque(owner);
    }
    Some(stateid) => {
      args.bool(false);
      stateid.encode(args);
      args.u32(0);
    }
  }
}

/// Runs PUTFH `fh` then one lock operation built by `build`: the status and the lock operation's body.
fn lock_call(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  fh: &[u8],
  build: impl FnOnce(&mut XdrWriter),
) -> (u32, Vec<u8>) {
  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(fh);
  build(&mut args);
  let reply = Client::call(service, server, args.as_slice());
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTFH);
  body.u32().unwrap(); // the lock operation's number
  body.u32().unwrap(); // and its status, which the frame already carries
  (reply.status, body.rest().to_vec())
}

/// A LOCK of `(kind, offset, length)` for the lock-owner `owner`, new to the open `open`, which must be
/// granted: its lock state id.
fn grant(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  fh: &[u8],
  (kind, offset, length): (u32, u64, u64),
  open: Stateid,
  owner: &[u8],
) -> Stateid {
  let (status, body) = lock_call(client, service, server, fh, |args| {
    lock_args(args, kind, offset, length, open, None, owner);
  });
  assert_eq!(status, Nfsstat4::Ok.wire(), "the lock is granted");
  Stateid::decode(&mut XdrReader::new(&body)).unwrap()
}

/// A `LOCK4denied`: (offset, length, type, client, owner).
fn denial(body: &[u8]) -> (u64, u64, u32, u64, Vec<u8>) {
  let mut denied = XdrReader::new(body);
  (
    denied.u64().unwrap(),
    denied.u64().unwrap(),
    denied.u32().unwrap(),
    denied.u64().unwrap(),
    denied.opaque(1024).unwrap().to_vec(),
  )
}

/// LOCKU of `range` under the lock state `held`: the status.
fn unlock(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  fh: &[u8],
  held: Stateid,
  (offset, length): (u64, u64),
) -> u32 {
  lock_call(client, service, server, fh, |args| {
    args.u32(op::LOCKU);
    args.u32(WRITE_LT);
    args.u32(0);
    held.encode(args);
    args.u64(offset);
    args.u64(length);
  })
  .0
}

/// CLOSE of the open `open` of `fh`: the status.
fn close_status(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  fh: &[u8],
  open: Stateid,
) -> u32 {
  let mut close = client.sequenced(2);
  close.u32(op::PUTFH);
  close.opaque(fh);
  close.u32(op::CLOSE);
  close.u32(0);
  open.encode(&mut close);
  Client::call(service, server, close.as_slice()).status
}

/// LOCKT of a read lock on byte 5 for `owner`: the status.
fn test_lock(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  fh: &[u8],
  owner: &[u8],
) -> u32 {
  lock_call(client, service, server, fh, |args| {
    args.u32(op::LOCKT);
    args.u32(READ_LT);
    args.u64(5);
    args.u64(1);
    args.u64(0);
    args.opaque(owner);
  })
  .0
}

/// A-35 (RFC 8881 §9, §18.10–18.12): two lock-owners of one client on one file. The first holds a write
/// lock; the second's LOCK over it is `NFS4ERR_DENIED`, reporting the holder's range, type, client and
/// owner, and LOCKT answers the same, while the holder's own LOCKT is clear. After the holder's LOCKU
/// the second's lock is granted. CLOSE of an open whose lock-owner still holds a lock is
/// `NFS4ERR_LOCKS_HELD`; once unlocked it closes and its lock state goes with it.
#[test]
fn a_lock_conflicts_with_another_owner_and_holds_its_open() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let owner = client.owner.clone();
  let (open, fh) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "db",
    BOTH,
    DENY_NONE,
  )
  .unwrap();

  let held = grant(
    &mut client,
    &mut service,
    &mut server,
    &fh,
    (WRITE_LT, 0, 10),
    open,
    b"writer",
  );
  let (status, body) = lock_call(&mut client, &mut service, &mut server, &fh, |args| {
    lock_args(args, READ_LT, 5, 1, open, None, b"reader");
  });
  assert_eq!(
    (status, denial(&body)),
    (
      Nfsstat4::Denied.wire(),
      (0, 10, WRITE_LT, client.clientid, b"writer".to_vec())
    ),
    "the conflict reports the holder's range, type, client and owner"
  );

  assert_eq!(
    test_lock(&mut client, &mut service, &mut server, &fh, b"reader"),
    Nfsstat4::Denied.wire(),
    "LOCKT reports the conflict"
  );
  assert_eq!(
    test_lock(&mut client, &mut service, &mut server, &fh, b"writer"),
    Nfsstat4::Ok.wire(),
    "LOCKT excludes the caller's own locks"
  );
  assert_eq!(
    close_status(&mut client, &mut service, &mut server, &fh, open),
    Nfsstat4::LocksHeld.wire(),
    "an open whose lock-owner holds a lock is not closed"
  );
  assert_eq!(
    unlock(&mut client, &mut service, &mut server, &fh, held, (0, 10)),
    Nfsstat4::Ok.wire(),
    "LOCKU"
  );
  let reader_lock = grant(
    &mut client,
    &mut service,
    &mut server,
    &fh,
    (READ_LT, 5, 1),
    open,
    b"reader",
  );
  assert!(
    read(&mut client, &mut service, &mut server, &fh, reader_lock).is_ok(),
    "a lock state id serves READ"
  );
  assert_eq!(
    unlock(
      &mut client,
      &mut service,
      &mut server,
      &fh,
      reader_lock,
      (5, 1)
    ),
    Nfsstat4::Ok.wire()
  );
  assert_eq!(
    close_status(&mut client, &mut service, &mut server, &fh, open),
    Nfsstat4::Ok.wire()
  );
  assert_eq!(
    service.file_states().unwrap().lock_state_count(),
    0,
    "the lock states went with the open"
  );
}

/// A-35 (RFC 8881 §18.10.3–18.10.4): a write lock under an open for reading is `NFS4ERR_OPENMODE`; a
/// reclaim, with no grace period to run in, is `NFS4ERR_NO_GRACE`; a zero length and an end past the
/// largest offset are `NFS4ERR_INVAL`; FREE_STATEID of a lock state holding a lock is
/// `NFS4ERR_LOCKS_HELD`.
#[test]
fn a_lock_is_refused_outside_its_rules() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let owner = client.owner.clone();
  let (read_open, fh) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "f",
    READ,
    DENY_NONE,
  )
  .unwrap();
  let (status, _) = lock_call(&mut client, &mut service, &mut server, &fh, |args| {
    lock_args(args, WRITE_LT, 0, 1, read_open, None, b"o");
  });
  assert_eq!(status, Nfsstat4::Openmode.wire());

  let (status, _) = lock_call(&mut client, &mut service, &mut server, &fh, |args| {
    args.u32(op::LOCK);
    args.u32(READ_LT);
    args.bool(true); // reclaim
    args.u64(0);
    args.u64(1);
    args.bool(true);
    args.u32(0);
    read_open.encode(args);
    args.u32(0);
    args.u64(0);
    args.opaque(b"o");
  });
  assert_eq!(status, Nfsstat4::NoGrace.wire());

  for (offset, length) in [(0, 0), (u64::MAX - 1, 2)] {
    let (status, _) = lock_call(&mut client, &mut service, &mut server, &fh, |args| {
      lock_args(args, READ_LT, offset, length, read_open, None, b"o");
    });
    assert_eq!(
      status,
      Nfsstat4::Inval.wire(),
      "offset {offset} length {length}"
    );
  }

  let (status, body) = lock_call(&mut client, &mut service, &mut server, &fh, |args| {
    lock_args(args, READ_LT, 0, u64::MAX, read_open, None, b"o");
  });
  assert_eq!(
    status,
    Nfsstat4::Ok.wire(),
    "a read lock to the end of the file"
  );
  let held = Stateid::decode(&mut XdrReader::new(&body)).unwrap();
  let mut free = client.sequenced(1);
  free.u32(op::FREE_STATEID);
  held.encode(&mut free);
  assert_eq!(
    Client::call(&mut service, &mut server, free.as_slice()).status,
    Nfsstat4::LocksHeld.wire()
  );
}

/// `NFS4_CONTENT_DATA` and `NFS4_CONTENT_HOLE` (RFC 7862 `data_content4`).
const CONTENT_DATA: u32 = 0;
const CONTENT_HOLE: u32 = 1;
/// Shape: the gap between a sparse file's two writes: several chunk windows.
const GAP: u64 = 1 << 20;

/// A file at the root with `head` at offset 0 and `tail` at [`GAP`], leaving a hole between.
fn sparse_file(bridge: &mut VolumeBridge<'_>, name: &str) {
  let cx = root_cx();
  let root = bridge.root(&cx).unwrap();
  let (attr, _) = bridge
    .create(ObjectId::new(root, 0), &cx, name, 0o644, 0)
    .unwrap();
  let file = ObjectId::new(attr.ino, attr.generation);
  bridge.write(file, &cx, 0, b"head").unwrap();
  bridge.write(file, &cx, GAP, b"tail").unwrap();
}

/// PUTROOTFH, LOOKUP `name`, then `op` built by `build`: the status and `op`'s body.
fn at_root_file(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  name: &str,
  build: impl FnOnce(&mut XdrWriter),
) -> (u32, Vec<u8>) {
  let mut args = client.sequenced(3);
  args.u32(op::PUTROOTFH);
  args.u32(op::LOOKUP);
  args.opaque(name.as_bytes());
  build(&mut args);
  let reply = Client::call(service, server, args.as_slice());
  if reply.status != Nfsstat4::Ok.wire() {
    return (reply.status, Vec::new());
  }
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  expect_ok(&mut body, op::LOOKUP);
  body.fixed(8).unwrap();
  (reply.status, body.rest().to_vec())
}

/// SEEK from `offset` for data or a hole: the status and `(sr_eof, sr_offset)`.
fn seek(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  offset: u64,
  what: u32,
) -> (u32, Option<(bool, u64)>) {
  let (status, body) = at_root_file(client, service, server, "sparse", |args| {
    args.u32(op::SEEK);
    Stateid::default().encode(args);
    args.u64(offset);
    args.u32(what);
  });
  if status != Nfsstat4::Ok.wire() {
    return (status, None);
  }
  let mut body = XdrReader::new(&body);
  (status, Some((body.bool().unwrap(), body.u64().unwrap())))
}

/// A-35 (RFC 7862 §15.11): over a file with data at 0 and at [`GAP`], SEEK finds the hole after the
/// first write and the data at the second; the end of the file is a hole reported with `sr_eof`; an
/// offset at or past the end is `NFS4ERR_NXIO`.
#[test]
fn seek_finds_the_holes_and_the_data_of_a_sparse_file() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  sparse_file(&mut bridge, "sparse");
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  client.minor = 2;
  let size = GAP + 4;

  let (_, hole) = seek(&mut client, &mut service, &mut server, 0, CONTENT_HOLE);
  let (eof, hole) = hole.unwrap();
  assert!(
    !eof && (4..GAP).contains(&hole),
    "the hole after the first write: {hole}"
  );
  let (_, data) = seek(&mut client, &mut service, &mut server, hole, CONTENT_DATA);
  assert_eq!(data, Some((false, GAP)), "the data of the second write");
  let (_, end) = seek(&mut client, &mut service, &mut server, GAP, CONTENT_HOLE);
  assert_eq!(end, Some((true, size)), "the end is a hole, with sr_eof");
  let (status, _) = seek(&mut client, &mut service, &mut server, size, CONTENT_DATA);
  assert_eq!(status, Nfsstat4::Nxio.wire());
}

/// A-35 (RFC 7862 §15.10): READ_PLUS of the whole sparse file returns its contents in order — the first
/// write's data, then the hole reported whole, then the second write's data — contiguous, with
/// `rpr_eof` set only once the contents reach the end.
#[test]
fn read_plus_returns_data_and_whole_holes_in_order() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  sparse_file(&mut bridge, "sparse");
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  client.minor = 2;
  let size = GAP + 4;
  let mut at = 0u64;
  let mut contents: Vec<(u32, u64, u64, Vec<u8>)> = Vec::new();
  let mut eof = false;
  while !eof {
    let (status, body) = at_root_file(&mut client, &mut service, &mut server, "sparse", |args| {
      args.u32(op::READ_PLUS);
      Stateid::default().encode(args);
      args.u64(at);
      args.u32(u32::MAX);
    });
    assert_eq!(status, Nfsstat4::Ok.wire());
    let mut body = XdrReader::new(&body);
    eof = body.bool().unwrap();
    for _ in 0..body.u32().unwrap() {
      let kind = body.u32().unwrap();
      let offset = body.u64().unwrap();
      let (length, data) = match kind {
        CONTENT_DATA => {
          let data = body.opaque(1 << 20).unwrap().to_vec();
          (u64::try_from(data.len()).unwrap(), data)
        }
        _ => (body.u64().unwrap(), Vec::new()),
      };
      assert_eq!(offset, at, "the contents are contiguous");
      at = offset + length;
      contents.push((kind, offset, length, data));
    }
    assert!(
      eof || at < size,
      "a reply short of the end does not claim it"
    );
  }
  assert_eq!(at, size);
  let data: Vec<u8> = contents
    .iter()
    .flat_map(|(_, _, _, data)| data.clone())
    .collect();
  assert!(
    data.starts_with(b"head") && data.ends_with(b"tail"),
    "both writes' bytes"
  );
  assert!(
    contents
      .iter()
      .any(|(kind, offset, length, _)| *kind == CONTENT_HOLE
        && *offset < GAP
        && offset + length == GAP),
    "the gap is one whole hole ending at the second write: {:?}",
    contents
      .iter()
      .map(|(kind, offset, length, _)| (kind, offset, length))
      .collect::<Vec<_>>()
  );
}

/// A-35 (RFC 8881 §16.2.3): an NFSv4.2 operation in a 4.1 compound is `NFS4ERR_OP_ILLEGAL`.
#[test]
fn a_v4_2_operation_is_illegal_in_a_v4_1_compound() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  sparse_file(&mut bridge, "sparse");
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let (status, _) = seek(&mut client, &mut service, &mut server, 0, CONTENT_DATA);
  assert_eq!(status, Nfsstat4::OpIllegal.wire());
}

/// COPY of `count` bytes at `offset` from `source` to the same offset of `destination` (both at the
/// root): the status and the bytes copied.
fn copy(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  (source, destination): (&str, &str),
  (offset, count): (u64, u64),
) -> (u32, Option<u64>) {
  let mut args = client.sequenced(6);
  args.u32(op::PUTROOTFH);
  args.u32(op::LOOKUP);
  args.opaque(source.as_bytes());
  args.u32(op::SAVEFH);
  args.u32(op::PUTROOTFH);
  args.u32(op::LOOKUP);
  args.opaque(destination.as_bytes());
  args.u32(op::COPY);
  Stateid::default().encode(&mut args);
  Stateid::default().encode(&mut args);
  args.u64(offset);
  args.u64(offset);
  args.u64(count);
  args.bool(true);
  args.bool(true);
  args.u32(0);
  let reply = Client::call(service, server, args.as_slice());
  if reply.status != Nfsstat4::Ok.wire() {
    return (reply.status, None);
  }
  let mut body = reply.walk();
  skip_sequence(&mut body);
  for opnum in [
    op::PUTROOTFH,
    op::LOOKUP,
    op::SAVEFH,
    op::PUTROOTFH,
    op::LOOKUP,
    op::COPY,
  ] {
    expect_ok(&mut body, opnum);
  }
  assert_eq!(
    body.u32().unwrap(),
    0,
    "no callback id: the copy was synchronous"
  );
  (reply.status, Some(body.u64().unwrap()))
}

/// Copies all of `source` into `destination` the way `copy_file_range` does: the first COPY asks for
/// everything (a count of zero), each further one continues from the bytes copied so far. Each must make
/// progress, so the loop is bounded by the length. The bytes copied.
fn copy_all(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  length: u64,
) -> u64 {
  let mut copied = 0u64;
  for _ in 0..length {
    if copied == length {
      break;
    }
    let count = if copied == 0 { 0 } else { length - copied };
    let (status, done) = copy(
      client,
      service,
      server,
      ("source", "destination"),
      (copied, count),
    );
    assert_eq!(status, Nfsstat4::Ok.wire(), "COPY");
    let done = done.unwrap();
    assert!(done > 0, "each COPY makes progress");
    copied += done;
  }
  copied
}

/// A-35 (RFC 7862 §15.2): COPY copies the source into the destination server-side, in as many COPYs as
/// the per-COPY bound needs; the destination then reads the source's bytes. A copy of a file onto
/// itself, and one reaching past the source's end, are `NFS4ERR_INVAL`.
#[test]
fn copy_moves_the_bytes_server_side_and_refuses_bad_ranges() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let cx = root_cx();
  let root = bridge.root(&cx).unwrap();
  let payload: Vec<u8> = (0..(3 * 1024 * 1024 / 7))
    .flat_map(|n: u32| n.to_le_bytes())
    .collect();
  let (source, _) = bridge
    .create(ObjectId::new(root, 0), &cx, "source", 0o644, 0)
    .unwrap();
  bridge
    .write(
      ObjectId::new(source.ino, source.generation),
      &cx,
      0,
      &payload,
    )
    .unwrap();
  let (destination, _) = bridge
    .create(ObjectId::new(root, 0), &cx, "destination", 0o644, 0)
    .unwrap();
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  client.minor = 2;
  let length = u64::try_from(payload.len()).unwrap();
  assert_eq!(
    copy_all(&mut client, &mut service, &mut server, length),
    length
  );
  let (status, _) = copy(
    &mut client,
    &mut service,
    &mut server,
    ("source", "source"),
    (0, 0),
  );
  assert_eq!(status, Nfsstat4::Inval.wire(), "onto itself");
  let (status, _) = copy(
    &mut client,
    &mut service,
    &mut server,
    ("source", "destination"),
    (0, length + 1),
  );
  assert_eq!(status, Nfsstat4::Inval.wire(), "past the source's end");
  drop(service);
  let mut got = Vec::new();
  let mut offset = 0u64;
  while offset < length {
    let before = got.len();
    bridge
      .read(
        ObjectId::new(destination.ino, destination.generation),
        &cx,
        offset,
        u32::MAX,
        &mut got,
      )
      .unwrap();
    assert!(got.len() > before, "the destination reads on");
    offset = u64::try_from(got.len()).unwrap();
  }
  assert!(got == payload, "the destination holds the source's bytes");
}

/// §4.6 A-33, A-35: an NFSv4 client carries extended attributes itself, so a `._name` is an ordinary
/// name to it, never the AppleDouble view of `name`'s attributes. Beside `notes` (which has an
/// attribute), a LOOKUP of `._notes` is `NFS4ERR_NOENT`; an OPEN that creates `._notes` makes a real
/// file, which a READDIR lists; and `notes`'s attribute is untouched.
#[test]
fn an_nfsv4_client_never_reaches_an_appledouble_view() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let notes = vol
    .create_file_no(&mut store, root, "notes", 0o644)
    .unwrap();
  vol
    .xattr_set(
      &mut store,
      notes,
      b"user.tag",
      b"kept",
      slates_vfs::xattr::XattrSet::Either,
    )
    .unwrap();
  {
    let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
    let mut service = export(&mut bridge, 0);
    let mut server = Server::standalone();
    let mut client = Client::connect(&mut service, &mut server, b"host-a");
    let (status, _) = at_root_file(&mut client, &mut service, &mut server, "._notes", |args| {
      args.u32(op::GETFH);
    });
    assert_eq!(status, Nfsstat4::Noent.wire(), "no view is looked up");
    let owner = client.owner.clone();
    open_at_root(
      &mut client,
      &mut service,
      &mut server,
      &owner,
      "._notes",
      BOTH,
      DENY_NONE,
    )
    .expect("a real `._notes` is created");
    let mut names: Vec<String> = list_root(&mut client, &mut service, &mut server)
      .into_iter()
      .map(|(name, _)| name)
      .collect();
    names.sort();
    assert_eq!(
      names,
      ["._notes", "notes"],
      "the listing shows the real file"
    );
  }
  assert_eq!(
    vol.xattr_names(&store, notes).unwrap(),
    vec![b"user.tag".to_vec().into_boxed_slice()],
    "the owner's attributes are untouched"
  );
}

/// PUTROOTFH, LOOKUP `name`, then one extended attribute operation built by `build`: the status and its
/// body.
fn xattr_call(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  name: &str,
  build: impl FnOnce(&mut XdrWriter),
) -> (u32, Vec<u8>) {
  at_root_file(client, service, server, name, build)
}

/// SETXATTR of `key` to `value` with `how` (0 either, 1 create, 2 replace): the status.
fn setxattr(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  how: u32,
  key: &[u8],
  value: &[u8],
) -> u32 {
  xattr_call(client, service, server, "tagged", |args| {
    args.u32(op::SETXATTR);
    args.u32(how);
    args.opaque(key);
    args.opaque(value);
  })
  .0
}

/// GETXATTR of `key`: the status and the value.
fn getxattr(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  key: &[u8],
) -> (u32, Vec<u8>) {
  let (status, body) = xattr_call(client, service, server, "tagged", |args| {
    args.u32(op::GETXATTR);
    args.opaque(key);
  });
  if status != Nfsstat4::Ok.wire() {
    return (status, Vec::new());
  }
  (
    status,
    XdrReader::new(&body).opaque(1 << 20).unwrap().to_vec(),
  )
}

/// LISTXATTRS from `cookie` within `maxcount` bytes: the status, the next cookie, the keys and eof.
fn listxattrs(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  cookie: u64,
  maxcount: u32,
) -> (u32, u64, Vec<Vec<u8>>, bool) {
  let (status, body) = xattr_call(client, service, server, "tagged", |args| {
    args.u32(op::LISTXATTRS);
    args.u64(cookie);
    args.u32(maxcount);
  });
  if status != Nfsstat4::Ok.wire() {
    return (status, 0, Vec::new(), false);
  }
  let mut body = XdrReader::new(&body);
  let next = body.u64().unwrap();
  let keys = (0..body.u32().unwrap())
    .map(|_| body.opaque(255).unwrap().to_vec())
    .collect();
  (status, next, keys, body.bool().unwrap())
}

/// SETXATTR in each mode, then GETXATTR: create refuses a set key, replace a missing one.
fn set_in_each_mode(c: &mut Client, s: &mut Export<'_>, v: &mut Server) {
  let set = |c: &mut Client, s: &mut Export<'_>, v: &mut Server, how, key: &[u8], value: &[u8]| {
    setxattr(c, s, v, how, key, value)
  };
  let statuses = [
    set(c, s, v, 1, b"alpha", b"one"),
    set(c, s, v, 1, b"alpha", b"again"),
    set(c, s, v, 2, b"missing", b"x"),
    set(c, s, v, 2, b"alpha", b"two"),
    set(c, s, v, 0, b"beta", b"three"),
  ];
  let ok = Nfsstat4::Ok.wire();
  assert_eq!(
    statuses,
    [ok, Nfsstat4::Exist.wire(), Nfsstat4::Noxattr.wire(), ok, ok],
    "create, create of a set key, replace of a missing key, replace, either"
  );
  assert_eq!(getxattr(c, s, v, b"alpha"), (ok, b"two".to_vec()));
  assert_eq!(getxattr(c, s, v, b"gamma").0, Nfsstat4::Noxattr.wire());
}

/// LISTXATTRS: the user keys without the prefix; a `maxcount` that holds one name pages by the cookie;
/// one that holds none is `NFS4ERR_TOOSMALL`.
fn list_in_pages(c: &mut Client, s: &mut Export<'_>, v: &mut Server) {
  let (status, _, keys, eof) = listxattrs(c, s, v, 0, 4096);
  assert_eq!(
    (status, keys, eof),
    (
      Nfsstat4::Ok.wire(),
      vec![b"alpha".to_vec(), b"beta".to_vec()],
      true
    ),
    "user keys only, prefix stripped"
  );
  let (_, next, first, eof) = listxattrs(c, s, v, 0, 16 + 12);
  assert_eq!((first, eof), (vec![b"alpha".to_vec()], false));
  let (_, _, second, eof) = listxattrs(c, s, v, next, 16 + 12);
  assert_eq!((second, eof), (vec![b"beta".to_vec()], true));
  assert_eq!(listxattrs(c, s, v, 0, 16).0, Nfsstat4::Toosmall.wire());
}

/// REMOVEXATTR of a set key, then of the same key again (`NFS4ERR_NOXATTR`).
fn remove_twice(c: &mut Client, s: &mut Export<'_>, v: &mut Server) {
  let remove = |c: &mut Client, s: &mut Export<'_>, v: &mut Server| {
    xattr_call(c, s, v, "tagged", |args| {
      args.u32(op::REMOVEXATTR);
      args.opaque(b"alpha");
    })
    .0
  };
  let first = remove(c, s, v);
  let second = remove(c, s, v);
  assert_eq!(
    (first, second),
    (Nfsstat4::Ok.wire(), Nfsstat4::Noxattr.wire())
  );
}

/// A-35 (RFC 8276 §8.4): the extended attribute operations over NFSv4.2. SETXATTR in its three modes
/// (create refuses an existing key with `NFS4ERR_EXIST`, replace a missing one with `NFS4ERR_NOXATTR`),
/// GETXATTR of a set and of a missing key, REMOVEXATTR; a key is the volume's `user.<key>`, so a
/// non-user attribute set another way is not listed; LISTXATTRS pages by its cookie within `maxcount`.
#[test]
fn extended_attributes_are_set_read_listed_and_removed() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let tagged = vol
    .create_file_no(&mut store, root, "tagged", 0o644)
    .unwrap();
  vol
    .xattr_set(
      &mut store,
      tagged,
      b"com.apple.provenance",
      b"mac",
      slates_vfs::xattr::XattrSet::Either,
    )
    .unwrap();
  {
    let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
    let mut service = export(&mut bridge, 0);
    let mut server = Server::standalone();
    let mut client = Client::connect(&mut service, &mut server, b"host-a");
    client.minor = 2;
    let (c, s, v) = (&mut client, &mut service, &mut server);
    set_in_each_mode(c, s, v);
    list_in_pages(c, s, v);
    remove_twice(c, s, v);
  }
  let names: Vec<Vec<u8>> = vol
    .xattr_names(&store, tagged)
    .unwrap()
    .into_iter()
    .map(|name| name.to_vec())
    .collect();
  assert_eq!(
    names,
    [b"com.apple.provenance".to_vec(), b"user.beta".to_vec()],
    "the volume holds the key under the user namespace, beside the untouched non-user attribute"
  );
}

/// A-35 (RFC 8276 §8.5): ACCESS reports the extended attribute bits supported and grants them as the
/// file's permissions allow: a read-only file of another owner grants reading and listing its
/// attributes, not changing them.
#[test]
fn access_grants_the_extended_attribute_bits_by_the_file_permissions() {
  /// Format: `ACCESS4_READ`, `ACCESS4_XAREAD`, `ACCESS4_XAWRITE`, `ACCESS4_XALIST`.
  const READ_BIT: u32 = 0x1;
  const XAREAD: u32 = 0x40;
  const XAWRITE: u32 = 0x80;
  const XALIST: u32 = 0x100;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let cx = root_cx();
  let root = bridge.root(&cx).unwrap();
  bridge
    .create(ObjectId::new(root, 0), &cx, "tagged", 0o644, 0)
    .unwrap();
  let mut service = export(&mut bridge, 1000);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  client.minor = 2;
  let asked = READ_BIT | XAREAD | XAWRITE | XALIST;
  let (status, body) = xattr_call(&mut client, &mut service, &mut server, "tagged", |args| {
    args.u32(op::ACCESS);
    args.u32(asked);
  });
  assert_eq!(status, Nfsstat4::Ok.wire());
  let mut body = XdrReader::new(&body);
  assert_eq!(body.u32().unwrap(), asked, "every bit asked is evaluated");
  assert_eq!(
    body.u32().unwrap(),
    READ_BIT | XAREAD | XALIST,
    "not XAWRITE on another's read-only file"
  );
}

/// The two write-only times are in `supported_attrs` (so a client sends `utimensat`'s explicit times),
/// a GETATTR or READDIR that asks for one is `NFS4ERR_INVAL` (RFC 8881 §5.6), and a SETATTR of them
/// sets the times the next GETATTR reads. A-35.
#[test]
fn the_write_only_times_are_supported_settable_and_never_read() {
  /// Format: `FATTR4_SUPPORTED_ATTRS`, `FATTR4_TIME_ACCESS_SET`, `FATTR4_TIME_MODIFY`,
  /// `FATTR4_TIME_MODIFY_SET`; `SET_TO_CLIENT_TIME4`.
  const SUPPORTED_ATTRS: u32 = 0;
  const TIME_ACCESS_SET: u32 = 48;
  const TIME_MODIFY: u32 = 53;
  const TIME_MODIFY_SET: u32 = 54;
  const SET_TO_CLIENT_TIME4: u32 = 1;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-times");
  let owner = client.owner.clone();
  let (stateid, fh) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "times",
    BOTH,
    DENY_NONE,
  )
  .unwrap();

  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(&fh);
  args.u32(op::GETATTR);
  Bitmap::of(&[SUPPORTED_ATTRS]).encode(&mut args);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTFH);
  expect_ok(&mut body, op::GETATTR);
  Bitmap::decode(&mut body).unwrap();
  let mut values = XdrReader::new(body.opaque(1024).unwrap());
  let supported = Bitmap::decode(&mut values).unwrap();
  assert!(
    supported.has(TIME_ACCESS_SET) && supported.has(TIME_MODIFY_SET),
    "both write-only times are supported"
  );

  let root = root_handle(&mut client, &mut service, &mut server);
  for request in [op::GETATTR, op::READDIR] {
    let mut args = client.sequenced(2);
    args.u32(op::PUTFH);
    if request == op::GETATTR {
      args.opaque(&fh);
      args.u32(op::GETATTR);
    } else {
      args.opaque(&root);
      args.u32(op::READDIR);
      args.u64(0);
      args.fixed(&[0; 8]);
      args.u32(4096);
      args.u32(4096);
    }
    Bitmap::of(&[TIME_MODIFY_SET]).encode(&mut args);
    let reply = Client::call(&mut service, &mut server, args.as_slice());
    assert_eq!(
      reply.results.last(),
      Some(&(request, Nfsstat4::Inval.wire())),
      "a read of a write-only attribute is refused"
    );
  }

  let mut args = client.sequenced(3);
  args.u32(op::PUTFH);
  args.opaque(&fh);
  args.u32(op::SETATTR);
  stateid.encode(&mut args);
  Bitmap::of(&[TIME_MODIFY_SET]).encode(&mut args);
  let mut values = XdrWriter::new();
  values.u32(SET_TO_CLIENT_TIME4);
  values.u64(1_100_000_000);
  values.u32(456_000_000);
  args.opaque(values.as_slice());
  args.u32(op::GETATTR);
  Bitmap::of(&[TIME_MODIFY]).encode(&mut args);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire(), "SETATTR then GETATTR");
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTFH);
  expect_ok(&mut body, op::SETATTR);
  assert!(
    Bitmap::decode(&mut body).unwrap().has(TIME_MODIFY_SET),
    "the reply names the time it set"
  );
  expect_ok(&mut body, op::GETATTR);
  Bitmap::decode(&mut body).unwrap();
  let mut values = XdrReader::new(body.opaque(1024).unwrap());
  assert_eq!(
    (values.u64().unwrap(), values.u32().unwrap()),
    (1_100_000_000, 456_000_000),
    "the modification time set is the one read"
  );
}

/// The root's file handle, from PUTROOTFH then GETFH.
fn root_handle(client: &mut Client, service: &mut Export<'_>, server: &mut Server) -> Vec<u8> {
  let mut args = client.sequenced(2);
  args.u32(op::PUTROOTFH);
  args.u32(op::GETFH);
  let reply = Client::call(service, server, args.as_slice());
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  expect_ok(&mut body, op::GETFH);
  body.opaque(128).unwrap().to_vec()
}

/// A volume whose wall clock never moves: every stamp repeats, as a coarse or stepped-back clock's do.
fn frozen_volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(slates_vfs::clock::StepClock::new(1, 0)),
    },
  )
  .unwrap()
}

/// The `change` attribute of `fh`.
fn change_of(client: &mut Client, service: &mut Export<'_>, server: &mut Server, fh: &[u8]) -> u64 {
  /// Format: `FATTR4_CHANGE`.
  const CHANGE: u32 = 3;
  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(fh);
  args.u32(op::GETATTR);
  Bitmap::of(&[CHANGE]).encode(&mut args);
  let reply = Client::call(service, server, args.as_slice());
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTFH);
  expect_ok(&mut body, op::GETATTR);
  Bitmap::decode(&mut body).unwrap();
  XdrReader::new(body.opaque(64).unwrap()).u64().unwrap()
}

/// Reads a `change_info4`: (atomic, before, after).
fn change_info(body: &mut XdrReader<'_>) -> (bool, u64, u64) {
  (
    body.bool().unwrap(),
    body.u64().unwrap(),
    body.u64().unwrap(),
  )
}

/// A-38: under a wall clock that never moves, `change` still moves on every write to a file (the
/// change time repeats; the counter does not), so a client caching the file sees every write.
#[test]
fn change_moves_on_every_write_under_a_frozen_clock() {
  let mut store = store();
  let mut vol = frozen_volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-change");
  let owner = client.owner.clone();
  let (stateid, fh) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "counted",
    BOTH,
    DENY_NONE,
  )
  .unwrap();
  let mut last = change_of(&mut client, &mut service, &mut server, &fh);
  for round in 0..3u8 {
    let mut args = client.sequenced(2);
    args.u32(op::PUTFH);
    args.opaque(&fh);
    args.u32(op::WRITE);
    stateid.encode(&mut args);
    args.u64(0);
    args.u32(FILE_SYNC);
    args.opaque(&[round; 3]);
    let reply = Client::call(&mut service, &mut server, args.as_slice());
    assert_eq!(reply.status, Nfsstat4::Ok.wire(), "WRITE {round}");
    let now = change_of(&mut client, &mut service, &mut server, &fh);
    assert!(now > last, "write {round} moved change ({last} -> {now})");
    last = now;
  }
}

/// Runs one namespace operation (`op`, its arguments written by `args`) on the directory `dir` and
/// returns its `change_info4` with the directory's `change` just before and just after the call.
fn namespace_change(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  dir: &[u8],
  (opnum, write_args): (u32, &dyn Fn(&mut XdrWriter)),
) -> ((bool, u64, u64), u64, u64) {
  let before = change_of(client, service, server, dir);
  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(dir);
  args.u32(opnum);
  write_args(&mut args);
  let reply = Client::call(service, server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire(), "op {opnum}");
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTFH);
  expect_ok(&mut body, opnum);
  let cinfo = change_info(&mut body);
  (cinfo, before, change_of(client, service, server, dir))
}

/// A-38: under a wall clock that never moves, a directory's CREATE and REMOVE answer an atomic
/// `change_info4` whose `before` is the directory's `change` just before the call and whose `after`
/// is the one the next GETATTR reads — so a client that cached the directory keeps its cache across
/// its own change and sees every other.
#[test]
fn create_and_remove_answer_atomic_change_info_under_a_frozen_clock() {
  /// Format: `NF4DIR`.
  const NF4DIR: u32 = 2;
  let mut store = store();
  let mut vol = frozen_volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-cinfo");
  let root = root_handle(&mut client, &mut service, &mut server);
  let create = |args: &mut XdrWriter| {
    args.u32(NF4DIR);
    args.opaque(b"sub");
    Bitmap::default().encode(args);
    args.opaque(&[]);
  };
  let remove = |args: &mut XdrWriter| args.opaque(b"sub");
  for (name, opnum, write_args) in [
    ("CREATE", op::CREATE, &create as &dyn Fn(&mut XdrWriter)),
    ("REMOVE", op::REMOVE, &remove),
  ] {
    let ((atomic, cinfo_before, cinfo_after), before, after) = namespace_change(
      &mut client,
      &mut service,
      &mut server,
      &root,
      (opnum, write_args),
    );
    assert!(atomic, "{name}: the change info is atomic");
    assert_eq!(
      cinfo_before, before,
      "{name}: before is the change before the call"
    );
    assert_eq!(
      cinfo_after, after,
      "{name}: after is the change the next GETATTR reads"
    );
    assert!(
      after > before,
      "{name}: the call moved the directory's change"
    );
  }
}

/// Runs PUTFH `fh` then one operation (`opnum`, its arguments written by `args`) and returns the
/// compound's overall status (the operation's, as it is the last).
fn one_op(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  fh: &[u8],
  opnum: u32,
  write_args: &dyn Fn(&mut XdrWriter),
) -> u32 {
  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(fh);
  args.u32(opnum);
  write_args(&mut args);
  Client::call(service, server, args.as_slice()).status
}

/// One operation to run, how to write its arguments, the status it must answer, and what it checks.
type OpCase<'a> = (u32, &'a dyn Fn(&mut XdrWriter), Nfsstat4, &'a str);

/// RFC 8881 §17's REQUIRED VERIFY and NVERIFY, which this server had answered `NFS4ERR_NOTSUPP`
/// (A-35 audit): each compares the named attributes with the object's, answering `NFS4ERR_NOT_SAME` or
/// `NFS4ERR_SAME` as it must, and a write-only attribute is `NFS4ERR_INVAL`.
#[test]
fn verify_and_nverify_compare_the_objects_attributes() {
  /// Format: `FATTR4_MODE`, `FATTR4_TIME_MODIFY_SET`.
  const MODE: u32 = 33;
  const TIME_MODIFY_SET: u32 = 54;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-verify");
  let owner = client.owner.clone();
  let (_, fh) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "verified",
    BOTH,
    DENY_NONE,
  )
  .unwrap();
  let mode_is = |mode: u32| {
    move |args: &mut XdrWriter| {
      Bitmap::of(&[MODE]).encode(args);
      let mut values = XdrWriter::new();
      values.u32(mode);
      args.opaque(values.as_slice());
    }
  };
  let write_only = |args: &mut XdrWriter| {
    Bitmap::of(&[TIME_MODIFY_SET]).encode(args);
    args.opaque(&[0; 16]);
  };
  // `open_at_root` creates with mode 0o644.
  let cases: [OpCase; 5] = [
    (
      op::VERIFY,
      &mode_is(0o644),
      Nfsstat4::Ok,
      "VERIFY of the mode it has",
    ),
    (
      op::VERIFY,
      &mode_is(0o600),
      Nfsstat4::NotSame,
      "VERIFY of another mode",
    ),
    (
      op::NVERIFY,
      &mode_is(0o644),
      Nfsstat4::Same,
      "NVERIFY of the mode it has",
    ),
    (
      op::NVERIFY,
      &mode_is(0o600),
      Nfsstat4::Ok,
      "NVERIFY of another mode",
    ),
    (
      op::VERIFY,
      &write_only,
      Nfsstat4::Inval,
      "VERIFY of a write-only attribute",
    ),
  ];
  for (opnum, write_args, expected, what) in cases {
    let status = one_op(
      &mut client,
      &mut service,
      &mut server,
      &fh,
      opnum,
      write_args,
    );
    assert_eq!(status, expected.wire(), "{what}");
  }
}

/// RFC 8881 §17's REQUIRED SECINFO, BACKCHANNEL_CTL and SET_SSV, which this server had answered
/// `NFS4ERR_NOTSUPP` (A-35 audit): SECINFO names the flavors for an existing name and refuses a missing
/// one, BACKCHANNEL_CTL accepts AUTH_SYS and refuses an RPCSEC_GSS handle it never issued, and SET_SSV
/// is refused without SP4_SSV.
#[test]
fn secinfo_backchannel_ctl_and_set_ssv_are_served() {
  /// Format: `AUTH_SYS` and `RPCSEC_GSS` flavors.
  const AUTH_SYS: u32 = 1;
  const RPCSEC_GSS: u32 = 6;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-secinfo-ops");
  let root = root_handle(&mut client, &mut service, &mut server);
  let owner = client.owner.clone();
  open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "named",
    BOTH,
    DENY_NONE,
  )
  .unwrap();
  let backchannel = |flavor: u32| {
    move |args: &mut XdrWriter| {
      args.u32(0x4000_0000); // callback program
      args.u32(1);
      args.u32(flavor);
      if flavor == AUTH_SYS {
        args.u32(0); // stamp
        args.opaque(b"host"); // machine name
        args.u32(0); // uid
        args.u32(0); // gid
        args.u32(0); // no supplementary groups
      } else {
        args.u32(1); // service
        args.opaque(b"server handle");
        args.opaque(b"client handle");
      }
    }
  };
  let set_ssv = |args: &mut XdrWriter| {
    args.opaque(b"secret");
    args.opaque(b"digest");
  };
  let cases: [OpCase; 5] = [
    (
      op::SECINFO,
      &|args: &mut XdrWriter| args.opaque(b"named"),
      Nfsstat4::Ok,
      "SECINFO of an existing name",
    ),
    (
      op::SECINFO,
      &|args: &mut XdrWriter| args.opaque(b"absent"),
      Nfsstat4::Noent,
      "SECINFO of a missing name",
    ),
    (
      op::BACKCHANNEL_CTL,
      &backchannel(AUTH_SYS),
      Nfsstat4::Ok,
      "BACKCHANNEL_CTL with AUTH_SYS",
    ),
    (
      op::BACKCHANNEL_CTL,
      &backchannel(RPCSEC_GSS),
      Nfsstat4::Noent,
      "no RPCSEC_GSS handle was issued",
    ),
    (
      op::SET_SSV,
      &set_ssv,
      Nfsstat4::Inval,
      "SET_SSV without SP4_SSV",
    ),
  ];
  for (opnum, write_args, expected, what) in cases {
    let status = one_op(
      &mut client,
      &mut service,
      &mut server,
      &root,
      opnum,
      write_args,
    );
    assert_eq!(status, expected.wire(), "{what}");
  }
}

/// SECINFO consumes the current file handle (RFC 8881 §2.6.3.1.1.8): a GETFH after it has none.
#[test]
fn secinfo_consumes_the_current_file_handle() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-secinfo");
  let owner = client.owner.clone();
  open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "named",
    BOTH,
    DENY_NONE,
  )
  .unwrap();
  let mut args = client.sequenced(3);
  args.u32(op::PUTROOTFH);
  args.u32(op::SECINFO);
  args.opaque(b"named");
  args.u32(op::GETFH);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(
    reply.results.last(),
    Some(&(op::GETFH, Nfsstat4::Nofilehandle.wire())),
    "the handle was consumed"
  );
}

/// A WRITE of `data` at 0 of `fh` under `stateid`: the compound's status.
fn write_status(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  fh: &[u8],
  stateid: Stateid,
) -> u32 {
  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(fh);
  args.u32(op::WRITE);
  stateid.encode(&mut args);
  args.u64(0);
  args.u32(FILE_SYNC);
  args.opaque(b"w");
  Client::call(service, server, args.as_slice()).status
}

/// A SETATTR of `fh` under `stateid` setting the size (`Some`) or the mode: the compound's status.
fn setattr_status(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  (fh, stateid): (&[u8], Stateid),
  size: Option<u64>,
) -> u32 {
  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(fh);
  args.u32(op::SETATTR);
  stateid.encode(&mut args);
  let mut values = XdrWriter::new();
  match size {
    Some(size) => {
      Bitmap::of(&[ATTR_SIZE]).encode(&mut args);
      values.u64(size);
    }
    None => {
      Bitmap::of(&[ATTR_MODE]).encode(&mut args);
      values.u32(0o640);
    }
  }
  args.opaque(values.as_slice());
  Client::call(service, server, args.as_slice()).status
}

/// RFC 8881 §9.1.2 (A-38): a state id's access mode governs write-type operations — a read-only
/// open's state id is refused a WRITE and a truncating SETATTR (`NFS4ERR_OPENMODE`, which the Linux
/// client turns into `EACCES` for an `O_RDONLY|O_TRUNC` open) yet sets other attributes; and a special
/// state id holds no share reservation, so another open's DENY_WRITE refuses its WRITE
/// (`NFS4ERR_LOCKED`) while its READ still reads, and a write-only open's READ is refused past another
/// open's DENY_READ.
#[test]
fn a_state_ids_access_mode_and_other_opens_denials_govern_io() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-modes");
  let (reader_state, fh) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    b"reader",
    "governed",
    READ,
    DENY_NONE,
  )
  .unwrap();
  assert_eq!(
    write_status(&mut client, &mut service, &mut server, &fh, reader_state),
    Nfsstat4::Openmode.wire(),
    "a read-only open may not write"
  );
  let with = (fh.as_slice(), reader_state);
  assert_eq!(
    setattr_status(&mut client, &mut service, &mut server, with, Some(0)),
    Nfsstat4::Openmode.wire(),
    "nor truncate"
  );
  assert_eq!(
    setattr_status(&mut client, &mut service, &mut server, with, None),
    Nfsstat4::Ok.wire(),
    "but may change the mode"
  );

  // Another owner denies writing: an anonymous WRITE is refused, an anonymous READ reads.
  open_at_root(
    &mut client,
    &mut service,
    &mut server,
    b"denier",
    "governed",
    READ,
    WRITE,
  )
  .unwrap();
  assert_eq!(
    write_status(
      &mut client,
      &mut service,
      &mut server,
      &fh,
      Stateid::ANONYMOUS
    ),
    Nfsstat4::Locked.wire(),
    "a special state id is refused past another open's DENY_WRITE"
  );
  assert!(
    read(
      &mut client,
      &mut service,
      &mut server,
      &fh,
      Stateid::ANONYMOUS
    )
    .is_ok(),
    "an anonymous READ is not denied"
  );

  // A write-only open reads, until another open denies reading.
  let (writer_state, other) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    b"writer",
    "second",
    WRITE,
    DENY_NONE,
  )
  .unwrap();
  assert!(
    read(&mut client, &mut service, &mut server, &other, writer_state).is_ok(),
    "a write-only open may read"
  );
  open_at_root(
    &mut client,
    &mut service,
    &mut server,
    b"read-denier",
    "second",
    WRITE,
    READ,
  )
  .unwrap();
  assert_eq!(
    read(&mut client, &mut service, &mut server, &other, writer_state),
    Err(Nfsstat4::Locked.wire()),
    "a write-only open's READ is refused past another open's DENY_READ"
  );
}

/// RFC 8881 §5.8 / RFC 8276 §8.1 (A-38): the supported attributes follow the compound's minor version
/// — an NFSv4.1 compound is never told of `xattr_support`, which NFSv4.2 defines, and an NFSv4.2 one
/// is — and a GETATTR of a 4.2 attribute in a 4.1 compound returns nothing for it.
#[test]
fn the_supported_attributes_follow_the_minor_version() {
  /// Format: `FATTR4_SUPPORTED_ATTRS`, `FATTR4_XATTR_SUPPORT`.
  const SUPPORTED_ATTRS: u32 = 0;
  const XATTR_SUPPORT: u32 = 82;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-minor");
  for (minor, offered) in [(1, false), (2, true)] {
    client.minor = minor;
    let mut args = client.sequenced(2);
    args.u32(op::PUTROOTFH);
    args.u32(op::GETATTR);
    Bitmap::of(&[SUPPORTED_ATTRS, XATTR_SUPPORT]).encode(&mut args);
    let reply = Client::call(&mut service, &mut server, args.as_slice());
    assert_eq!(reply.status, Nfsstat4::Ok.wire(), "GETATTR in 4.{minor}");
    let mut body = reply.walk();
    skip_sequence(&mut body);
    expect_ok(&mut body, op::PUTROOTFH);
    expect_ok(&mut body, op::GETATTR);
    let returned = Bitmap::decode(&mut body).unwrap();
    let mut values = XdrReader::new(body.opaque(1024).unwrap());
    let supported = Bitmap::decode(&mut values).unwrap();
    assert_eq!(
      supported.has(XATTR_SUPPORT),
      offered,
      "4.{minor} supported_attrs"
    );
    assert_eq!(
      returned.has(XATTR_SUPPORT),
      offered,
      "4.{minor} returned bitmap"
    );
  }
}

/// An EXCLUSIVE4_1 OPEN (share both) of `name` at the root with `verifier` and `attrs` (a bitmap and
/// its values), then GETFH: the compound's status and, on success, the file handle.
fn exclusive_open(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  (name, verifier): (&str, [u8; 8]),
  (bits, values): (&[u32], &[u8]),
) -> (u32, Option<(Vec<u8>, Bitmap)>) {
  /// Format: `OPEN4_CREATE`, `EXCLUSIVE4_1`, `CLAIM_NULL`.
  const OPEN4_CREATE: u32 = 1;
  const EXCLUSIVE4_1: u32 = 3;
  const CLAIM_NULL: u32 = 0;
  let mut args = client.sequenced(3);
  args.u32(op::PUTROOTFH);
  args.u32(op::OPEN);
  args.u32(0);
  args.u32(BOTH);
  args.u32(DENY_NONE);
  args.u64(client.clientid);
  args.opaque(&client.owner);
  args.u32(OPEN4_CREATE);
  args.u32(EXCLUSIVE4_1);
  args.fixed(&verifier);
  Bitmap::of(bits).encode(&mut args);
  args.opaque(values);
  args.u32(CLAIM_NULL);
  args.opaque(name.as_bytes());
  args.u32(op::GETFH);
  let reply = Client::call(service, server, args.as_slice());
  if reply.status != Nfsstat4::Ok.wire() {
    return (reply.status, None);
  }
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  let (_, attrset) = open_result(&mut body);
  expect_ok(&mut body, op::GETFH);
  (
    reply.status,
    Some((body.opaque(128).unwrap().to_vec(), attrset)),
  )
}

/// RFC 8881 §18.16.3 (A-38): an EXCLUSIVE4_1 create keeps its verifier in the new file's times and
/// sets its other attributes; a retry of the same create — from a new server's session, as after a
/// daemon restart, when the slot cache that made retries exactly-once is gone — opens the file it made,
/// another verifier's create is `NFS4ERR_EXIST`, and times in the create's attributes are
/// `NFS4ERR_INVAL`: `suppattr_exclcreat` names the mode and leaves the times out.
#[test]
fn an_exclusive_create_is_retried_after_a_restart_onto_its_own_file() {
  /// Format: `FATTR4_SUPPATTR_EXCLCREAT`, `FATTR4_TIME_ACCESS_SET`, `FATTR4_TIME_MODIFY_SET`;
  /// `SET_TO_SERVER_TIME4`.
  const SUPPATTR_EXCLCREAT: u32 = 75;
  const TIME_ACCESS_SET: u32 = 48;
  const TIME_MODIFY_SET: u32 = 54;
  const SET_TO_SERVER_TIME4: u32 = 0;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let verifier = [7, 6, 5, 4, 3, 2, 1, 0];
  let mode = 0o600u32.to_be_bytes();
  let first = {
    let mut server = Server::standalone();
    let mut client = Client::connect(&mut service, &mut server, b"host-exclusive");
    let root = root_handle(&mut client, &mut service, &mut server);
    let mut args = client.sequenced(2);
    args.u32(op::PUTFH);
    args.opaque(&root);
    args.u32(op::GETATTR);
    Bitmap::of(&[SUPPATTR_EXCLCREAT]).encode(&mut args);
    let reply = Client::call(&mut service, &mut server, args.as_slice());
    let mut body = reply.walk();
    skip_sequence(&mut body);
    expect_ok(&mut body, op::PUTFH);
    expect_ok(&mut body, op::GETATTR);
    Bitmap::decode(&mut body).unwrap();
    let exclcreat = Bitmap::decode(&mut XdrReader::new(body.opaque(64).unwrap())).unwrap();
    assert!(
      exclcreat.has(ATTR_MODE),
      "the mode is set by an exclusive create"
    );
    assert!(
      !exclcreat.has(TIME_MODIFY_SET),
      "the times hold the verifier"
    );
    let (status, fh) = exclusive_open(
      &mut client,
      &mut service,
      &mut server,
      ("once", verifier),
      (&[ATTR_MODE], &mode),
    );
    assert_eq!(status, Nfsstat4::Ok.wire(), "the exclusive create");
    let (fh, attrset) = fh.unwrap();
    assert!(
      attrset.has(ATTR_MODE) && attrset.has(TIME_MODIFY_SET) && attrset.has(TIME_ACCESS_SET),
      "the reply names the mode it set and the times that hold the verifier, which the client resets"
    );
    fh
  };

  // A new server: the sessions and slot caches of the first are gone; the volume is not.
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-exclusive-again");
  let (status, again) = exclusive_open(
    &mut client,
    &mut service,
    &mut server,
    ("once", verifier),
    (&[ATTR_MODE], &mode),
  );
  assert_eq!(status, Nfsstat4::Ok.wire(), "the retried create opens");
  assert_eq!(again.unwrap().0, first, "the file the first create made");
  let (status, _) = exclusive_open(
    &mut client,
    &mut service,
    &mut server,
    ("once", [1; 8]),
    (&[ATTR_MODE], &mode),
  );
  assert_eq!(status, Nfsstat4::Exist.wire(), "another verifier's create");
  let (status, _) = exclusive_open(
    &mut client,
    &mut service,
    &mut server,
    ("twice", verifier),
    (&[TIME_MODIFY_SET], &SET_TO_SERVER_TIME4.to_be_bytes()),
  );
  assert_eq!(
    status,
    Nfsstat4::Inval.wire(),
    "times in an exclusive create's attributes"
  );
}

/// RFC 8881 §3.3.1 (A-38): `nfstime4` carries signed 64-bit seconds, and the volume keeps nanoseconds in
/// an `i64`, so a SETATTR of a time past 2^32 seconds (the year 2106) or before 1970 sets exactly that
/// time and GETATTR reads it back — NFSv3's 32-bit `nfstime3` does not limit NFSv4.
#[test]
fn times_past_2106_and_before_1970_round_trip() {
  /// Format: `FATTR4_TIME_ACCESS`, `FATTR4_TIME_ACCESS_SET`, `FATTR4_TIME_MODIFY`,
  /// `FATTR4_TIME_MODIFY_SET`; `SET_TO_CLIENT_TIME4`.
  const TIME_ACCESS: u32 = 47;
  const TIME_ACCESS_SET: u32 = 48;
  const TIME_MODIFY: u32 = 53;
  const TIME_MODIFY_SET: u32 = 54;
  const SET_TO_CLIENT_TIME4: u32 = 1;
  /// Shape: 2^32 seconds (2106) and a day before the epoch.
  const LATE: i64 = 1 << 32;
  const EARLY: i64 = -86_400;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-y2106");
  let owner = client.owner.clone();
  let (stateid, fh) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "timeless",
    BOTH,
    DENY_NONE,
  )
  .unwrap();
  let mut args = client.sequenced(3);
  args.u32(op::PUTFH);
  args.opaque(&fh);
  args.u32(op::SETATTR);
  stateid.encode(&mut args);
  Bitmap::of(&[TIME_ACCESS_SET, TIME_MODIFY_SET]).encode(&mut args);
  let mut values = XdrWriter::new();
  values.u32(SET_TO_CLIENT_TIME4);
  values.i64(EARLY);
  values.u32(250);
  values.u32(SET_TO_CLIENT_TIME4);
  values.i64(LATE);
  values.u32(500);
  args.opaque(values.as_slice());
  args.u32(op::GETATTR);
  Bitmap::of(&[TIME_ACCESS, TIME_MODIFY]).encode(&mut args);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire(), "SETATTR then GETATTR");
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTFH);
  expect_ok(&mut body, op::SETATTR);
  Bitmap::decode(&mut body).unwrap();
  expect_ok(&mut body, op::GETATTR);
  Bitmap::decode(&mut body).unwrap();
  let mut values = XdrReader::new(body.opaque(64).unwrap());
  let atime = (values.i64().unwrap(), values.u32().unwrap());
  let mtime = (values.i64().unwrap(), values.u32().unwrap());
  assert_eq!(atime, (EARLY, 250), "a time before 1970 reads back");
  assert_eq!(mtime, (LATE, 500), "a time past 2106 reads back");
}

/// RFC 8881 §2.10.6.4, §18.46.3 (A-39): the session's negotiated sizes are the contract. `maxread`
/// and `maxwrite` are the transfer ceiling, and a WRITE of exactly that is served (the Linux client
/// had been told 4 KiB more and every large write came back `NFS4ERR_INVAL`); a request past the
/// negotiated request size is `NFS4ERR_REQ_TOO_BIG` and more operations than negotiated
/// `NFS4ERR_TOO_MANY_OPS`, before the slot is used (the next sequence id still serves); a reply the
/// client asked to be kept past the negotiated cache size is `NFS4ERR_REP_TOO_BIG_TO_CACHE` on the
/// operation that would carry it there.
#[test]
fn the_sessions_negotiated_sizes_bound_requests_and_replies() {
  /// Format: `FATTR4_MAXREAD`, `FATTR4_MAXWRITE`.
  const MAXREAD: u32 = 30;
  const MAXWRITE: u32 = 31;
  /// Shape: the cache size this server offers: past a SEQUENCE and a PUTFH's result, short of a READ
  /// of a page.
  const CACHED: u32 = 512;
  let transfer = slates_bridge_nfs::procedures::MAX_TRANSFER;
  let size = transfer + COMPOUND_HEADER_BYTES;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::new(
    7,
    Limits {
      max_clients: 4,
      max_sessions_per_client: 1,
      offer: ChannelAttrs {
        header_pad: 0,
        max_request: size,
        max_response: size,
        max_response_cached: CACHED,
        max_operations: 8,
        max_requests: 1,
      },
      lease_ns: u64::MAX,
    },
  );
  let mut client = Client::connect(&mut service, &mut server, b"host-sizes");
  let root = root_handle(&mut client, &mut service, &mut server);
  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(&root);
  args.u32(op::GETATTR);
  Bitmap::of(&[MAXREAD, MAXWRITE]).encode(&mut args);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTFH);
  expect_ok(&mut body, op::GETATTR);
  Bitmap::decode(&mut body).unwrap();
  let mut values = XdrReader::new(body.opaque(64).unwrap());
  let limits = (values.u64().unwrap(), values.u64().unwrap());
  assert_eq!(
    limits,
    (u64::from(transfer), u64::from(transfer)),
    "maxread and maxwrite are the transfer ceiling"
  );

  let owner = client.owner.clone();
  let (stateid, fh) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "sized",
    BOTH,
    DENY_NONE,
  )
  .unwrap_or_else(|status| panic!("open: {status}"));
  let write = |client: &mut Client, len: usize| {
    let mut args = client.sequenced(2);
    args.u32(op::PUTFH);
    args.opaque(&fh);
    args.u32(op::WRITE);
    stateid.encode(&mut args);
    args.u64(0);
    args.u32(FILE_SYNC);
    args.opaque(&vec![7u8; len]);
    args
  };
  let args = write(&mut client, usize::try_from(transfer).unwrap());
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire(), "a WRITE of maxwrite");

  let args = write(&mut client, usize::try_from(size).unwrap());
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(
    reply.status,
    Nfsstat4::ReqTooBig.wire(),
    "a request past the negotiated size"
  );
  // The refused request used no sequence id: the next one is the same number again.
  client.sequence -= 1;
  let mut args = client.sequenced(9);
  for _ in 0..9 {
    args.u32(op::PUTROOTFH);
  }
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(
    reply.status,
    Nfsstat4::TooManyOps.wire(),
    "more operations than negotiated"
  );
  client.sequence -= 1;

  assert_eq!(
    read(&mut client, &mut service, &mut server, &fh, stateid),
    Err(Nfsstat4::RepTooBigToCache.wire()),
    "a kept reply past the negotiated cache size"
  );
}

/// Format: the `type` attribute's number (RFC 8881 §5.8.1.3).
const ATTR_TYPE: u32 = 1;
/// Shape: files in the listed directory: enough that a page of small entries cannot hold them all, within the
/// fixture's inode cap.
const LISTED: usize = 240;
/// Shape: the client's `maxcount` for the page: Linux's own lower bound on a READDIR buffer, a page.
const LIST_MAXCOUNT: u32 = 4096;

/// RFC 8881 §18.23 (A-90): do list a directory of 240 files with one READDIR asking only `type` and `fileid` (the
/// attributes a Linux `ls` asks for) under a 4 KiB `maxcount`; expect the page to be full — no further entry would
/// have fit — since the listing has not ended. A page sized by the v3 READDIRPLUS budget it is built from (whose
/// entries carry a whole `fattr3` and a handle) held a third of that, and cost a listing three times the round trips.
#[test]
fn a_readdir_page_of_small_entries_fills_the_clients_maxcount() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  for at in 0..LISTED {
    vol
      .create_file_no(&mut store, root, &format!("f{at:04}"), 0o644)
      .unwrap();
  }
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let mut args = client.sequenced(2);
  args.u32(op::PUTROOTFH);
  args.u32(op::READDIR);
  args.u64(0);
  args.fixed(&[0; 8]);
  args.u32(LIST_MAXCOUNT);
  args.u32(LIST_MAXCOUNT);
  Bitmap::of(&[ATTR_TYPE, ATTR_FILEID]).encode(&mut args);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire(), "READDIR");
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  expect_ok(&mut body, op::READDIR);
  body.fixed(8).unwrap();
  // The result's own bytes: the verifier, then each entry, then the two closing booleans.
  let mut used = 8 + 4 + 4;
  let mut largest = 0;
  let mut returned = 0;
  while body.bool().unwrap() {
    body.u64().unwrap();
    let name = body.string(255).unwrap().len();
    let bitmap = Bitmap::decode(&mut body).unwrap();
    assert!(bitmap.has(ATTR_TYPE) && bitmap.has(ATTR_FILEID));
    let values = body.opaque(1024).unwrap().len();
    let pad = |len: usize| len.div_ceil(4) * 4;
    // value-follows, cookie, the name, the bitmap (its length and two words), the values.
    let entry = 4 + 8 + 4 + pad(name) + 4 + 4 * 2 + 4 + pad(values);
    used += entry;
    largest = largest.max(entry);
    returned += 1;
  }
  let eof = body.bool().unwrap();
  assert!(
    !eof,
    "{returned} of {LISTED} entries cannot be the whole listing"
  );
  assert!(
    used + largest > LIST_MAXCOUNT as usize,
    "the page held {returned} entries in {used} of {LIST_MAXCOUNT} bytes; another {largest}-byte entry would fit"
  );
}

/// One READDIR of the root directory from `cookie` with `verf` under `maxcount`: the status, the verifier, each
/// entry's (cookie, name), and `eof`.
fn readdir_page(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  (cookie, verf, maxcount): (u64, [u8; 8], u32),
) -> (u32, [u8; 8], Vec<(u64, String)>, bool) {
  let mut args = client.sequenced(2);
  args.u32(op::PUTROOTFH);
  args.u32(op::READDIR);
  args.u64(cookie);
  args.fixed(&verf);
  args.u32(maxcount);
  args.u32(maxcount);
  Bitmap::of(&[ATTR_TYPE, ATTR_FILEID]).encode(&mut args);
  let reply = Client::call(service, server, args.as_slice());
  if reply.status != Nfsstat4::Ok.wire() {
    return (reply.status, [0; 8], Vec::new(), false);
  }
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  expect_ok(&mut body, op::READDIR);
  let verf: [u8; 8] = body.fixed(8).unwrap().try_into().unwrap();
  let mut entries = Vec::new();
  while body.bool().unwrap() {
    let cookie = body.u64().unwrap();
    let name = body.string(255).unwrap().to_owned();
    Bitmap::decode(&mut body).unwrap();
    body.opaque(1024).unwrap();
    entries.push((cookie, name));
  }
  (reply.status, verf, entries, body.bool().unwrap())
}

/// Shape: a `maxcount` that holds a few dozen small entries, so a 240-file listing takes several pages.
const SMALL_PAGE: u32 = 1024;
/// Shape: a `maxcount` too small for any entry: the verifier and the closing booleans leave no room.
const NO_ROOM: u32 = 24;

/// RFC 8881 §18.23 (A-95: the page is encoded at the directory's owner). Do: list a directory of 240 files in 1 KiB
/// pages, each resuming from the last entry's cookie under the verifier the first page returned; then start again,
/// create a file between two pages, and resume; then ask with a `maxcount` that holds no entry. Expect: every name
/// exactly once, no dot entry, every cookie above the reserved 2 and its own, `eof` on the last
/// page only and several pages; a resume after the directory changed refused `NFS4ERR_BAD_COOKIE` (the verifier is
/// the directory's change version, as the v3 listing's is); and the empty page `NFS4ERR_TOOSMALL`.
#[test]
fn a_directory_listed_in_pages_returns_every_name_once_and_refuses_a_stale_resume() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  for at in 0..LISTED {
    vol
      .create_file_no(&mut store, root, &format!("f{at:04}"), 0o644)
      .unwrap();
  }
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let (c, s, v) = (&mut client, &mut service, &mut server);
  let (names, cookies, pages) = list_in_small_pages(c, s, v);
  assert!(
    pages > 2,
    "a 240-file listing in 1 KiB pages took {pages} pages"
  );
  assert!(
    cookies.iter().all(|cookie| *cookie > 2),
    "cookies 0, 1 and 2 are reserved"
  );
  let mut sorted = names.clone();
  sorted.sort();
  let expected: Vec<String> = (0..LISTED).map(|at| format!("f{at:04}")).collect();
  assert_eq!(sorted, expected, "every name exactly once, no dot entry");
  let mut seen = cookies.clone();
  seen.sort_unstable();
  seen.dedup();
  assert_eq!(seen.len(), cookies.len(), "every entry's cookie is its own");
  a_stale_resume_and_a_page_with_no_room_are_refused(c, s, v);
}

/// The whole root listing in [`SMALL_PAGE`] pages, each resumed from the last cookie under the returned verifier:
/// the names, their cookies, and the page count.
fn list_in_small_pages(
  c: &mut Client,
  s: &mut Export<'_>,
  v: &mut Server,
) -> (Vec<String>, Vec<u64>, usize) {
  let mut names = Vec::new();
  let mut cookies = Vec::new();
  let (mut cookie, mut verf, mut pages) = (0u64, [0u8; 8], 0);
  loop {
    let (status, page_verf, entries, eof) = readdir_page(c, s, v, (cookie, verf, SMALL_PAGE));
    assert_eq!(status, Nfsstat4::Ok.wire(), "page {pages}");
    assert!(
      !entries.is_empty() || eof,
      "a page that does not end the listing carries an entry"
    );
    verf = page_verf;
    pages += 1;
    for (entry_cookie, name) in entries {
      cookies.push(entry_cookie);
      names.push(name);
    }
    if eof {
      return (names, cookies, pages);
    }
    cookie = *cookies.last().unwrap();
  }
}

/// A resume after a create between pages is `NFS4ERR_BAD_COOKIE`; a page with no room for an entry `NFS4ERR_TOOSMALL`.
fn a_stale_resume_and_a_page_with_no_room_are_refused(
  c: &mut Client,
  s: &mut Export<'_>,
  v: &mut Server,
) {
  let (_, verf, first, _) = readdir_page(c, s, v, (0, [0; 8], SMALL_PAGE));
  let resume = first.last().unwrap().0;
  let mut args = c.sequenced(2);
  args.u32(op::PUTROOTFH);
  c.open_args(
    &mut args,
    &c.owner.clone(),
    "between-pages",
    BOTH,
    DENY_NONE,
    Some((GUARDED, Some(0o644), None)),
  );
  let reply = Client::call(s, v, args.as_slice());
  assert_eq!(
    reply.status,
    Nfsstat4::Ok.wire(),
    "the create between pages"
  );
  assert_eq!(
    readdir_page(c, s, v, (resume, verf, SMALL_PAGE)).0,
    Nfsstat4::BadCookie.wire(),
    "a resume under the verifier of a directory that has since changed"
  );
  assert_eq!(
    readdir_page(c, s, v, (0, [0; 8], NO_ROOM)).0,
    Nfsstat4::Toosmall.wire(),
    "a page with no room for an entry"
  );
}
