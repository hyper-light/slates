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
//! **Bounded** (A-745; §4.17.1 "A fetch's answer"). The answer is made a row at a time as it is sent
//! ([`crate::render::Streaming`]): a first walk counts the files and bytes its head names from the index's rows alone,
//! then each revlog is written a row a step, each step's bytes charged to its own room; a step past it is refused. Its
//! files are [`crate::render::revlogs`]' whole rendering, byte for byte.

use super::Refusal;
use super::getbundle::Store;
use crate::Admission;

/// Format: the answer when no stream is served (7.2's "operation forbidden by server").
const FORBIDDEN: &[u8] = b"1\n";

/// Writes `stream_out`'s answer (module docs) whole: [`crate::render::Streaming`]'s steps at once, within `admission`.
pub fn stream_out(
  store: &impl Store,
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let Some((index, content)) = store.revlogs() else {
    out.extend_from_slice(FORBIDDEN);
    return Ok(());
  };
  let mut streaming = crate::render::Streaming::default();
  while streaming
    .step(index, content, admission, out)
    .map_err(revlog_refused)?
  {}
  Ok(())
}

/// A rendering's refusal as a command's: past the admission as it is, any other as the store's.
pub(crate) fn revlog_refused(refused: crate::render::Refused) -> Refusal {
  match refused {
    crate::render::Refused::Past(past) => Refusal::PastAdmission(past),
    _ => Refusal::Store { what: "a revlog" },
  }
}
