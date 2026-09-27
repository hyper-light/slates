//! The NFSv4.2 operations (RFC 7862; A-35), each served through the v3 layer as the 4.1 operations
//! are: SEEK and READ_PLUS on the SEEK extension procedure (`crate::procedures::extension::SEEK`),
//! which asks the volume where data and holes are; COPY as a server-side copy through the v3 READ and
//! WRITE procedures, so the bytes never cross the wire; IO_ADVISE, answered with no hints acted on.
//!
//! Not offered (`NFS4ERR_NOTSUPP`, each for its reason):
//! - **CLONE**: a clone shares chunks between two files, and the volume releases a chunk by its one
//!   owning inode's birth epoch (§4.5, D-6), so a chunk shared across inodes needs the reference
//!   counts dedup brings (Phase 7).
//! - **ALLOCATE**: a reservation that makes later writes immune to `ENOSPC` cannot hold under
//!   copy-on-write, where a write into a snapshotted chunk takes new space.
//! - **DEALLOCATE**: punching a hole splits chunk-backed extents so that one chunk sits under two
//!   extents, which the volume's release accounting does not yet allow; it is its own volume change.
//! - **WRITE_SAME**, the layout operations, and the inter-server copy (a COPY naming source servers).
//!
//! The extended attribute operations (RFC 8276) run on the attribute extension procedures. The protocol
//! carries the user namespace only (§5), so a key `k` is the volume's attribute `user.k`, and
//! LISTXATTRS lists the `user.` attributes without the prefix: an attribute set over NFSv4 has the name
//! a Linux host's `setfattr -n user.k` gives it, through FUSE or on a landed file.

use super::Nfsstat4;
use super::compound::{
  Backend, Frame, Outcome, attrs_of, change_info, check_open_kind, check_state, current, op,
  state_io, v3,
};
use super::types::{Bitmap, Stateid};
use super::v3call;
use crate::nfs::{Nfsfh3, Wcc};
use crate::procedures::{MAX_TRANSFER, extension, extension_status, io_want, seek_what};
use crate::xdr::{XdrReader, XdrWriter};

/// Format: `stable_how4` `UNSTABLE4`: a COPY's writes are unstable, made stable by the client's
/// COMMIT, as a client's own WRITEs would be (RFC 7862 §15.2.3 `wr_committed`).
const UNSTABLE4: u32 = 0;
/// Format: the bytes one READ_PLUS content entry costs beyond its data: the content type, the offset,
/// and a length or an opaque's length word (RFC 7862 `read_plus_content`).
const CONTENT_ENTRY_BYTES: usize = 4 + 8 + 8;

/// One NFSv4.2 operation (RFC 7862), or one of RFC 8276's extended attribute operations.
pub(super) async fn operation<B: Backend>(
  backend: &mut B,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  match opnum {
    op::SEEK => seek(backend, reader, frame).await,
    op::READ_PLUS => read_plus(backend, reader, frame).await,
    op::COPY => copy(backend, reader, frame).await,
    op::IO_ADVISE => io_advise(reader, frame),
    _ => xattr_operation(backend, opnum, reader, frame).await,
  }
}

/// The volume's answer to "where is the next data (`data`) or hole at or after `offset`": whether one
/// was found, its offset, and the file's size.
async fn seek_in<B: Backend>(
  backend: &mut B,
  fh: &Nfsfh3,
  offset: u64,
  data: bool,
) -> Result<(bool, u64, u64), Nfsstat4> {
  let what = if data {
    seek_what::DATA
  } else {
    seek_what::HOLE
  };
  let result = backend
    .call_v3(extension::SEEK, v3call::seek_args(fh, offset, what))
    .await;
  v3(v3call::seek(&result))
}

/// SEEK (§15.11): an offset at or past the end is `NFS4ERR_NXIO`; data not found is `sr_eof`; the
/// virtual hole at the end of the file is reported with `sr_eof` set.
async fn seek<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  let stateid = Stateid::decode(reader).map_err(bad)?;
  let offset = reader.u64().map_err(bad)?;
  let data = match reader.u32().map_err(bad)? {
    seek_what::DATA => true,
    seek_what::HOLE => false,
    _ => return Err(Nfsstat4::Badxdr),
  };
  let fh = current(frame)?.clone();
  check_open_kind(backend, &fh).await?;
  check_state(backend, &fh, frame.clientid, &stateid, io_want::READ).await?;
  let (found, at, size) = seek_in(backend, &fh, offset, data).await?;
  if offset >= size {
    return Err(Nfsstat4::Nxio);
  }
  let mut body = XdrWriter::new();
  body.bool(!found || at >= size);
  body.u64(if found { at } else { size });
  Ok(body.into_bytes())
}

/// READ_PLUS (§15.10): the requested range as contiguous data and hole contents, holes whole (they may
/// start before and end after the request), data read through the v3 READ. The reply stops short of the
/// request at the session's reply size; `rpr_eof` is set only when the contents reach the end of the
/// file, so a short reply never claims the end.
async fn read_plus<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &Frame,
) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  let stateid = Stateid::decode(reader).map_err(bad)?;
  let offset = reader.u64().map_err(bad)?;
  let count = reader.u32().map_err(bad)?;
  let fh = current(frame)?.clone();
  check_open_kind(backend, &fh).await?;
  check_state(backend, &fh, frame.clientid, &stateid, io_want::READ).await?;
  let size = attrs_of(backend, &fh).await?.size;
  let budget = usize::try_from(backend.with_v4(|server| server.limits().offer.max_response)?)
    .unwrap_or(usize::MAX);
  let end = offset
    .saturating_add(u64::from(count.min(MAX_TRANSFER)))
    .min(size);
  let mut contents = Contents {
    writer: XdrWriter::new(),
    entries: 0,
    at: offset,
    budget,
  };
  while contents.at < end && contents.has_room() {
    if !next_content(
      backend,
      (&fh, frame.clientid, &stateid),
      &mut contents,
      size,
      end,
    )
    .await?
    {
      break;
    }
  }
  let mut body = XdrWriter::new();
  body.bool(contents.at >= size);
  body.u32(contents.entries);
  body.fixed(contents.writer.as_slice());
  Ok(body.into_bytes())
}

/// A READ_PLUS reply's contents as they are built: the encoded entries, their count, the next offset,
/// and the reply size they must fit.
struct Contents {
  writer: XdrWriter,
  entries: u32,
  at: u64,
  budget: usize,
}

impl Contents {
  /// Whether one more entry's fixed fields fit the reply.
  fn has_room(&self) -> bool {
    self.writer.len() + CONTENT_ENTRY_BYTES < self.budget
  }

  /// The data bytes the next entry may carry within the reply.
  fn room(&self) -> u64 {
    u64::try_from(
      self
        .budget
        .saturating_sub(self.writer.len() + CONTENT_ENTRY_BYTES),
    )
    .unwrap_or(0)
  }
}

/// Appends the content at `contents.at`: a whole hole up to the next data, or the data up to the next
/// hole (within `end` and the reply's room). `false` when nothing more can be added.
async fn next_content<B: Backend>(
  backend: &mut B,
  (fh, clientid, stateid): (&Nfsfh3, Option<u64>, &Stateid),
  contents: &mut Contents,
  size: u64,
  end: u64,
) -> Result<bool, Nfsstat4> {
  let at = contents.at;
  let (found, data_at, _) = seek_in(backend, fh, at, true).await?;
  let data_at = if found { data_at } else { size };
  if data_at > at {
    // A hole from here to the next data (or the end), reported whole.
    contents.writer.u32(seek_what::HOLE);
    contents.writer.u64(at);
    contents.writer.u64(data_at - at);
    contents.entries += 1;
    contents.at = data_at;
    return Ok(true);
  }
  let (_, hole_at, _) = seek_in(backend, fh, at, false).await?;
  let take = hole_at.min(end).saturating_sub(at).min(contents.room());
  if take == 0 {
    return Ok(false);
  }
  // The bytes are read under the READ_PLUS's state id at the owner, as a READ is (RFC 8881 §9.1.2).
  let result = state_io(
    backend,
    extension::READ_STATE,
    v3call::read_args(fh, at, u32::try_from(take).unwrap_or(MAX_TRANSFER)),
    clientid,
    stateid,
  )
  .await?;
  let (_, data) = v3(v3call::read(&result))?;
  if data.is_empty() {
    return Ok(false);
  }
  contents.writer.u32(seek_what::DATA);
  contents.writer.u64(at);
  contents.writer.opaque(&data);
  contents.entries += 1;
  contents.at = at.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
  Ok(true)
}

/// COPY (§15.2), intra-server and synchronous: the saved file's range read and written to the current
/// file through the v3 READ and WRITE, consecutively. One COPY copies at most what the client could
/// have in flight over its session at once (the request size × the slots), so the server does no more
/// work per compound than the client could ask of it while the wire carries none of the bytes; a
/// shorter copy is answered with its count, and the client continues from there (as `copy_file_range`
/// does). A failure after some bytes were copied answers the bytes copied.
async fn copy<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let request = CopyRequest::decode(reader)?;
  let source = frame.saved.clone().ok_or(Nfsstat4::Nofilehandle)?;
  let destination = current(frame)?.clone();
  if source == destination {
    return Err(Nfsstat4::Inval);
  }
  for fh in [&source, &destination] {
    check_open_kind(backend, fh)
      .await
      .map_err(|_| Nfsstat4::WrongType)?;
  }
  check_state(
    backend,
    &source,
    frame.clientid,
    &request.source_stateid,
    io_want::READ,
  )
  .await?;
  check_state(
    backend,
    &destination,
    frame.clientid,
    &request.destination_stateid,
    io_want::WRITE,
  )
  .await?;
  let size = attrs_of(backend, &source).await?.size;
  let count = if request.count == 0 {
    size.saturating_sub(request.source_offset)
  } else {
    request.count
  };
  if request.source_offset > size || request.source_offset.saturating_add(count) > size {
    return Err(Nfsstat4::Inval);
  }
  let limits = backend.with_v4(|server| server.limits())?;
  let bound =
    u64::from(limits.offer.max_request).saturating_mul(u64::from(limits.offer.max_requests));
  let (copied, verifier) = copy_range(
    backend,
    frame.clientid,
    (&source, request.source_offset, &request.source_stateid),
    (
      &destination,
      request.destination_offset,
      &request.destination_stateid,
    ),
    count.min(bound),
  )
  .await?;
  let mut body = XdrWriter::new();
  body.u32(0); // no callback id: the copy completed synchronously
  body.u64(copied);
  body.u32(UNSTABLE4);
  body.fixed(&verifier);
  body.bool(true); // consecutive
  body.bool(true); // synchronous
  Ok(body.into_bytes())
}

/// One side of a COPY: the file, the offset, and the state id its I/O runs under.
type CopySide<'a> = (&'a Nfsfh3, u64, &'a Stateid);

/// Copies up to `count` bytes from `source` to `destination` in transfer-sized pieces: the bytes copied
/// and the last write verifier. Each piece reads and writes under the COPY's state ids at the files'
/// owners, so the opens authorize them as they would a READ and a WRITE (RFC 8881 §9.1.2). The first
/// failure ends the copy: before any byte it is the COPY's status, after some it answers the bytes
/// copied.
async fn copy_range<B: Backend>(
  backend: &mut B,
  clientid: Option<u64>,
  source: CopySide<'_>,
  destination: CopySide<'_>,
  count: u64,
) -> Result<(u64, [u8; v3call::VERF_SIZE]), Nfsstat4> {
  let (source, source_offset, source_stateid) = source;
  let (destination, destination_offset, destination_stateid) = destination;
  let mut copied = 0u64;
  let mut verifier = [0u8; v3call::VERF_SIZE];
  while copied < count {
    let piece =
      u32::try_from((count - copied).min(u64::from(MAX_TRANSFER))).unwrap_or(MAX_TRANSFER);
    let step = copy_piece(
      backend,
      clientid,
      (source, source_offset + copied, source_stateid),
      (
        destination,
        destination_offset + copied,
        destination_stateid,
      ),
      piece,
    )
    .await;
    match step {
      Ok((0, _)) => break,
      Ok((written, written_verifier)) => {
        copied += u64::from(written);
        verifier = written_verifier;
      }
      Err(status) if copied == 0 => return Err(status),
      Err(_) => break,
    }
  }
  Ok((copied, verifier))
}

/// One READ of up to `piece` bytes from the source and its WRITE to the destination: the bytes
/// written and the write verifier.
async fn copy_piece<B: Backend>(
  backend: &mut B,
  clientid: Option<u64>,
  (source, source_offset, source_stateid): CopySide<'_>,
  (destination, destination_offset, destination_stateid): CopySide<'_>,
  piece: u32,
) -> Result<(u32, [u8; v3call::VERF_SIZE]), Nfsstat4> {
  let result = state_io(
    backend,
    extension::READ_STATE,
    v3call::read_args(source, source_offset, piece),
    clientid,
    source_stateid,
  )
  .await?;
  let (_, data) = v3(v3call::read(&result))?;
  if data.is_empty() {
    return Ok((0, [0u8; v3call::VERF_SIZE]));
  }
  let result = state_io(
    backend,
    extension::WRITE_STATE,
    v3call::write_args(destination, destination_offset, UNSTABLE4, &data),
    clientid,
    destination_stateid,
  )
  .await?;
  let (written, _, verifier) = v3(v3call::write(&result))?;
  Ok((written, verifier))
}

/// A decoded COPY.
struct CopyRequest {
  source_stateid: Stateid,
  destination_stateid: Stateid,
  source_offset: u64,
  destination_offset: u64,
  count: u64,
}

impl CopyRequest {
  /// Reads COPY4args. A COPY naming source servers is an inter-server copy, which this server does not
  /// offer (`NFS4ERR_NOTSUPP`).
  fn decode(reader: &mut XdrReader<'_>) -> Result<CopyRequest, Nfsstat4> {
    let bad = |_| Nfsstat4::Badxdr;
    let request = CopyRequest {
      source_stateid: Stateid::decode(reader).map_err(bad)?,
      destination_stateid: Stateid::decode(reader).map_err(bad)?,
      source_offset: reader.u64().map_err(bad)?,
      destination_offset: reader.u64().map_err(bad)?,
      count: reader.u64().map_err(bad)?,
    };
    let _consecutive = reader.bool().map_err(bad)?;
    let _synchronous = reader.bool().map_err(bad)?;
    if reader.u32().map_err(bad)? != 0 {
      return Err(Nfsstat4::Notsupp);
    }
    Ok(request)
  }
}

/// IO_ADVISE (§15.5): the hints are read and none is reported as acted on (an empty `ior_hints`), which
/// the protocol permits: the hints are advisory.
fn io_advise(reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  let _stateid = Stateid::decode(reader).map_err(bad)?;
  let _offset = reader.u64().map_err(bad)?;
  let _count = reader.u64().map_err(bad)?;
  let _hints = Bitmap::decode(reader).map_err(bad)?;
  current(frame)?;
  let mut body = XdrWriter::new();
  Bitmap::default().encode(&mut body);
  Ok(body.into_bytes())
}

/// Format: the namespace NFSv4 extended attribute keys name (RFC 8276 §5): the user namespace.
const USER_NAMESPACE: &[u8] = b"user.";
/// Format: the bytes a LISTXATTRS reply spends beyond its names: the cookie, the name count and the
/// end-of-list flag (RFC 8276 `LISTXATTRS4resok`).
const LISTXATTRS_FIXED_BYTES: usize = 8 + 4 + 4;

/// An RFC 8276 key read from the wire, as the volume's attribute name `user.<key>`.
fn user_name(reader: &mut XdrReader<'_>) -> Result<Vec<u8>, Nfsstat4> {
  let key = reader
    .opaque(slates_vfs::xattr::XATTR_NAME_MAX_BYTES - USER_NAMESPACE.len())
    .map_err(|_| Nfsstat4::Nametoolong)?;
  if key.is_empty() {
    return Err(Nfsstat4::Inval);
  }
  let mut name = USER_NAMESPACE.to_vec();
  name.extend_from_slice(key);
  Ok(name)
}

/// An attribute extension's status: OK, one of RFC 8276's two, or an NFSv3 status mapped.
fn extension_result(reader: &mut XdrReader<'_>) -> Result<(), Nfsstat4> {
  match reader.u32().map_err(|_| Nfsstat4::Serverfault)? {
    0 => Ok(()),
    extension_status::NOXATTR => Err(Nfsstat4::Noxattr),
    extension_status::XATTR2BIG => Err(Nfsstat4::Xattr2big),
    other => Err(
      crate::nfs::Nfsstat3::from_wire(other)
        .map(Nfsstat4::of_v3)
        .unwrap_or(Nfsstat4::Serverfault),
    ),
  }
}

/// GETXATTR, SETXATTR, LISTXATTRS and REMOVEXATTR (RFC 8276 §8.4).
async fn xattr_operation<B: Backend>(
  backend: &mut B,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  let fh = current(frame)?.clone();
  let mut args = XdrWriter::new();
  fh.encode(&mut args);
  let procedure = match opnum {
    op::GETXATTR => {
      args.opaque(&user_name(reader)?);
      extension::XATTR_GET
    }
    op::SETXATTR => {
      let how = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
      let name = user_name(reader)?;
      let limit = usize::try_from(backend.with_v4(|server| server.limits().offer.max_request)?)
        .unwrap_or(usize::MAX);
      let value = reader.opaque(limit).map_err(|_| Nfsstat4::Badxdr)?;
      args.u32(how);
      args.opaque(&name);
      args.opaque(value);
      extension::XATTR_SET
    }
    op::LISTXATTRS => return listxattrs(backend, reader, &fh).await,
    _ => {
      args.opaque(&user_name(reader)?);
      extension::XATTR_REMOVE
    }
  };
  let result = backend.call_v3(procedure, args.into_bytes()).await;
  let mut result = XdrReader::new(&result);
  extension_result(&mut result)?;
  match opnum {
    op::GETXATTR => {
      let value = result
        .opaque(usize::try_from(MAX_TRANSFER).unwrap_or(usize::MAX))
        .map_err(|_| Nfsstat4::Serverfault)?;
      let mut body = XdrWriter::new();
      body.opaque(value);
      Ok(body.into_bytes())
    }
    // SETXATTR and REMOVEXATTR answer the file's change_info4, from the one call that changed it.
    _ => {
      let wcc = Wcc::decode_with_change(&mut result).map_err(|_| Nfsstat4::Serverfault)?;
      Ok(change_info(&wcc))
    }
  }
}

/// LISTXATTRS (RFC 8276 §8.4.3): the `user.` attributes, prefix stripped, from the cookie (the index
/// of the next name in the ascending listing) while they fit `maxcount`; a first name that cannot fit
/// is `NFS4ERR_TOOSMALL`.
async fn listxattrs<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  fh: &Nfsfh3,
) -> Outcome {
  let cookie = reader.u64().map_err(|_| Nfsstat4::Badxdr)?;
  let maxcount = usize::try_from(reader.u32().map_err(|_| Nfsstat4::Badxdr)?).unwrap_or(usize::MAX);
  let mut args = XdrWriter::new();
  fh.encode(&mut args);
  let result = backend
    .call_v3(extension::XATTR_LIST, args.into_bytes())
    .await;
  let mut result = XdrReader::new(&result);
  extension_result(&mut result)?;
  let count = result.u32().map_err(|_| Nfsstat4::Serverfault)?;
  let mut keys: Vec<Vec<u8>> = Vec::new();
  for _ in 0..count {
    let name = result
      .opaque(slates_vfs::xattr::XATTR_NAME_MAX_BYTES)
      .map_err(|_| Nfsstat4::Serverfault)?;
    if let Some(key) = name.strip_prefix(USER_NAMESPACE) {
      keys.push(key.to_vec());
    }
  }
  let start = usize::try_from(cookie)
    .unwrap_or(usize::MAX)
    .min(keys.len());
  let mut names = XdrWriter::new();
  let mut returned = 0u32;
  let mut next = start;
  for key in &keys[start..] {
    let mut one = XdrWriter::new();
    one.opaque(key);
    if LISTXATTRS_FIXED_BYTES + names.len() + one.len() > maxcount {
      break;
    }
    names.fixed(one.as_slice());
    returned += 1;
    next += 1;
  }
  if returned == 0 && next < keys.len() {
    return Err(Nfsstat4::Toosmall);
  }
  let mut body = XdrWriter::new();
  body.u64(u64::try_from(next).unwrap_or(u64::MAX));
  body.u32(returned);
  body.fixed(names.as_slice());
  body.bool(next >= keys.len());
  Ok(body.into_bytes())
}
