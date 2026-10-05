//! READDIR (RFC 8881 §18.23) encoded at the directory's owner (§4.6 A-95): the `entry4` list is written straight from
//! the shared bridge's rows into the reply, inside the `extension::READDIR4` procedure, which routes by the directory's
//! handle like every other call and is served by the same export, under the same capability, permission and owner
//! gates.
//!
//! Before A-95 a page was a v3 READDIRPLUS: the owner encoded every entry's `fattr3` and handle, the front end decoded
//! them again (a `String` per name, a `Vec` per handle) and only then wrote the `entry4`s. Measured on Linux's own
//! NFSv4.2 client over loopback (2026-10-05, 8,300 entries, `lsloop`): four READDIRs per listing carrying 129 KB each,
//! as the kernel's nfsd's did (126 KB), but a round trip of 0.74 ms against nfsd's 0.34 ms. In the daemon's profile
//! the v3 encode was 36% of its samples and the decode and re-encode most of the rest of the compound's 62%.
//!
//! The pseudo-root is the one directory not listed here: its entries are volumes on other shards, which its owner
//! cannot describe, so the front end lists it through the gathered v3 listing (`compound::readdir`).
//!
//! The page's rules are the v3 listing's: the directory's change version is the cookie verifier; a page ends between
//! cookie groups, never inside one (`whole_cookie_groups`, AUD-29-86); a page with no room for its first group is
//! `NFS4ERR_TOOSMALL`; the dot entries the bridge synthesizes are not listed (RFC 8881 §18.23.4); and a v3 cookie
//! is shifted past v4's reserved 0, 1 and 2.

use slates_bridge_core::{DirEntry, whole_cookie_groups};

use super::Nfsstat4;
use super::attr::{self, FsFigures};
use super::types::{Bitmap, VERIFIER_SIZE};
use crate::nfs::{Fattr3, Nfsfh3};
use crate::xdr::{XdrReader, XdrWriter};

/// Format: the READDIR cookies 0, 1 and 2 are reserved (RFC 8881 §18.23.3), so a v3 cookie is shifted past them.
pub(crate) const COOKIE_SHIFT: u64 = 2;
/// Format: the size of one READDIR entry's fixed fields (value-follows, cookie, name length, the attribute bitmap's
/// length and the attribute list's length), the least an entry costs.
pub(crate) const ENTRY_FIXED: u32 = 4 + 8 + 4 + 4 + 4;
/// Shape: the bytes a listed entry typically encodes to (a short name and a few attributes), sizing a page's buffer up
/// front; a page of larger entries grows it, and the reply's `maxcount` caps it.
pub(crate) const ENTRY_TYPICAL: usize = 64;
/// Format: the reply's words beside its entries: the end-of-list and `eof` booleans.
const PAGE_TAIL: usize = 2 * size_of::<u32>();

/// One page asked of the directory's owner: the `extension::READDIR4` arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRequest {
  /// The directory.
  pub dir: Nfsfh3,
  /// The cookie to resume after, in the v3 numbering (the client's less [`COOKIE_SHIFT`]); 0 starts the listing.
  pub cookie: u64,
  /// The cookie verifier the client holds.
  pub verf: [u8; VERIFIER_SIZE],
  /// The client's reply budget (`maxcount`).
  pub maxcount: u32,
  /// The attributes each entry carries.
  pub requested: Bitmap,
  /// The compound's minor version.
  pub minor: u32,
  /// The directory's filesystem figures, which every entry of a volume shares.
  pub figures: FsFigures,
}

impl PageRequest {
  /// The request's wire form.
  pub fn encode(&self) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    self.dir.encode(&mut writer);
    writer.u64(self.cookie);
    writer.fixed(&self.verf);
    writer.u32(self.maxcount);
    self.requested.encode(&mut writer);
    writer.u32(self.minor);
    self.figures.encode(&mut writer);
    writer.into_bytes()
  }

  /// The request from its wire form: a bad handle `NFS4ERR_BADHANDLE`, anything else malformed `NFS4ERR_BADXDR`.
  pub fn decode(reader: &mut XdrReader<'_>) -> Result<PageRequest, Nfsstat4> {
    let bad = |_| Nfsstat4::Badxdr;
    let dir = Nfsfh3::decode(reader).map_err(|_| Nfsstat4::Badhandle)?;
    let cookie = reader.u64().map_err(bad)?;
    let mut verf = [0u8; VERIFIER_SIZE];
    verf.copy_from_slice(reader.fixed(VERIFIER_SIZE).map_err(bad)?);
    let maxcount = reader.u32().map_err(bad)?;
    let requested = Bitmap::decode(reader).map_err(bad)?;
    let minor = reader.u32().map_err(bad)?;
    let figures = FsFigures::decode(reader).map_err(bad)?;
    Ok(PageRequest {
      dir,
      cookie,
      verf,
      maxcount,
      requested,
      minor,
      figures,
    })
  }

  /// The least one entry of this request encodes to: value-follows, the cookie, the shortest name, and the requested
  /// attributes of a minimal object (A-90), never below [`ENTRY_FIXED`].
  pub fn entry_floor(&self) -> usize {
    entry_floor(&self.requested, self.minor)
  }

  /// How many rows to ask the bridge for: as many entries as the smallest could fill `maxcount` with, one more so the
  /// page's end shows, and the dot entries a listing from the start carries first.
  pub fn rows_wanted(&self) -> usize {
    let budget = usize::try_from(self.maxcount).unwrap_or(usize::MAX);
    let dots = match self.cookie {
      0 => 2,
      1 => 1,
      _ => 0,
    };
    (budget / self.entry_floor().max(1))
      .saturating_add(1)
      .saturating_add(dots)
  }
}

/// The least one READDIR entry encodes to under `requested` at minor version `minor` (the module doc).
pub(crate) fn entry_floor(requested: &Bitmap, minor: u32) -> usize {
  (size_of::<u32>() + size_of::<u64>() + 2 * size_of::<u32>())
    .saturating_add(attr::encoded_floor((requested, minor)))
    .max(usize::try_from(ENTRY_FIXED).unwrap_or(usize::MAX))
}

/// Writes the READDIR4resok body for `rows` (what the bridge returned for `request`, dot entries first) after
/// whatever `writer` already holds: the verifier `verf`, every entry that fits `maxcount` up to a whole cookie group,
/// and the end markers. `object_of` gives a row's attributes and handle. Refuses `NFS4ERR_TOOSMALL` when not even the
/// first group fits, and passes on `object_of`'s or the attribute encoder's refusal.
pub fn encode_page<F>(
  request: &PageRequest,
  verf: [u8; VERIFIER_SIZE],
  rows: &[DirEntry],
  mut object_of: F,
  writer: &mut XdrWriter,
) -> Result<(), Nfsstat4>
where
  F: FnMut(&DirEntry) -> Result<(Fattr3, Nfsfh3), Nfsstat4>,
{
  let budget = usize::try_from(request.maxcount).unwrap_or(usize::MAX);
  let dots = rows
    .iter()
    .take_while(|row| row.name == "." || row.name == "..")
    .count();
  let children = rows.get(dots..).unwrap_or(&[]);
  writer.fixed(&verf);
  let entries_start = writer.len();
  // Where each written entry begins, so a page cut back to a whole cookie group truncates at an entry's start.
  let mut starts: Vec<usize> = Vec::with_capacity(children.len());
  let mut one = XdrWriter::new();
  for row in children {
    let (attrs, handle) = object_of(row)?;
    one.clear();
    one.bool(true);
    one.u64(row.cookie.saturating_add(COOKIE_SHIFT));
    one.opaque(row.name.as_bytes());
    attr::encode(
      (&request.requested, request.minor),
      &attrs,
      &handle,
      &request.figures,
      &mut one,
    )?;
    let used = writer.len().saturating_sub(entries_start);
    if used
      .saturating_add(one.len())
      .saturating_add(PAGE_TAIL)
      .saturating_add(VERIFIER_SIZE)
      > budget
    {
      break;
    }
    starts.push(writer.len());
    writer.fixed(one.as_slice());
  }
  let fitted = starts.len();
  let sent = whole_cookie_groups(children, fitted);
  if sent == 0 && !children.is_empty() {
    return Err(Nfsstat4::Toosmall);
  }
  if let Some(cut) = starts.get(sent) {
    writer.truncate(*cut);
  }
  // The listing ended when every child the bridge had was sent and it had fewer rows than it was asked for.
  let eof = sent == children.len() && rows.len() < request.rows_wanted();
  writer.bool(false); // no more entries in this reply
  writer.bool(eof);
  Ok(())
}

/// The READDIR4resok body an `extension::READDIR4` result carries: the status word first, then (when it is OK) the
/// body. A result too short for its status, or with a status this server never sends, is a server fault.
pub fn page_of(mut result: Vec<u8>) -> Result<Vec<u8>, Nfsstat4> {
  let word = result
    .get(..size_of::<u32>())
    .and_then(|word| <[u8; 4]>::try_from(word).ok())
    .map(u32::from_be_bytes)
    .ok_or(Nfsstat4::Serverfault)?;
  match Nfsstat4::from_wire(word) {
    Some(Nfsstat4::Ok) => {
      result.drain(..size_of::<u32>());
      Ok(result)
    }
    Some(status) => Err(status),
    None => Err(Nfsstat4::Serverfault),
  }
}
