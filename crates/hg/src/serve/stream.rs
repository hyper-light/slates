//! `stream_out` (stream clones part 2; §4.17.3 "Stream clones: the revlog view"; 7.2's `wireprotov1server.stream`,
//! `streamclone.generatev1wireproto` and `generatev1` \[C\]): the repository's revlogs sent as their files, which a
//! client writes into its store as they are (`hg clone --stream` against a server listing `stream` or `streamreqs`).
//!
//! **The answer**, raw (7.2's `streamreslegacy`: over SSH as it is, over HTTP as `application/mercurial-0.1`): `0\n`,
//! then the files' count and their bytes' total (`<files> <bytes>\n`), then each file: its store path through
//! `encodedir`, a NUL, its size, a line end, and its bytes. Only files with bytes are sent, as 7.2 skips an empty one.
//! The files are A-706's rendering ([`crate::render::revlogs`]), in 7.2's walk order. Where no stream is served (a
//! secret changeset, [`Store::streams`]) the answer is `1\n`, 7.2's "operation forbidden by server".
//!
//! **Bounded.** The rendering and the answer are charged to the request's admission; one past it is refused out of
//! band. The answer is made whole before it is sent: a stream sliced as it is sent is owed with the rendering kept
//! as versions commit (GAPS).

use super::Refusal;
use super::getbundle::Store;
use crate::Admission;
use crate::decimal::Decimal;
use crate::read::to_u64;
use crate::store::encode_dir;

/// Format: the most bytes a count takes as decimal text (`u64::MAX`'s 20 digits).
const DECIMAL_BYTES: u64 = 20;
/// Format: the answer's first line when the stream follows.
const STREAMING: &[u8] = b"0\n";
/// Format: the answer when no stream is served (7.2's "operation forbidden by server").
const FORBIDDEN: &[u8] = b"1\n";

/// Writes `stream_out`'s answer (module docs).
pub fn stream_out(
  store: &impl Store,
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let Some(files) = store.stream_files(admission)? else {
    out.extend_from_slice(FORBIDDEN);
    return Ok(());
  };
  let sent: Vec<(Vec<u8>, Vec<u8>)> = files
    .into_iter()
    .filter(|(_, bytes)| !bytes.is_empty())
    .collect();
  let total = sent.iter().fold(0_u64, |total, (_, bytes)| {
    total.saturating_add(to_u64(bytes.len()))
  });
  // Each file's line: its name, a NUL, its size and a line end; and the first two lines.
  let heads = sent.iter().fold(0_u64, |heads, (name, _)| {
    heads
      .saturating_add(to_u64(name.len()))
      .saturating_add(DECIMAL_BYTES)
      .saturating_add(2)
  });
  // Format: the first two lines: `0` and a line end; the count, a space, the total and a line end.
  let lines = DECIMAL_BYTES.saturating_mul(2).saturating_add(4);
  admission.charge(total.saturating_add(heads).saturating_add(lines))?;
  out.extend_from_slice(STREAMING);
  out.extend_from_slice(Decimal::unsigned(to_u64(sent.len())).as_bytes());
  out.push(b' ');
  out.extend_from_slice(Decimal::unsigned(total).as_bytes());
  out.push(b'\n');
  for (name, bytes) in &sent {
    out.extend_from_slice(&encode_dir(name));
    out.push(0);
    out.extend_from_slice(Decimal::unsigned(to_u64(bytes.len())).as_bytes());
    out.push(b'\n');
    out.extend_from_slice(bytes);
  }
  Ok(())
}
