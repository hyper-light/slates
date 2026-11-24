//! The words a push's refusal is answered in, the one wording both edges write (A-683's SSH site, which dies with them
//! on standard error; the HTTP site, which writes them on git's band 3 once its reply's head is out, since the head's
//! `200` was written before the push was read, A-722): git's own where git dies with fixed ones \[C: git v2.50.1
//! `pkt-line.c` lines 158, 371, 433, 450 and 458; `receive-pack.c` lines 2052, 2111, 2143 and 2164\], without the bytes
//! git quotes after some, which the driver has read and no longer holds, and slates' own where git has none (a push past
//! its room, memory the system refused, a store or a shard not served here).

use slates_git::pktline::{self, HEADER_LEN};
use slates_git::receive::Refusal;

use super::PushFailed;

/// Format: git's words when its input ends inside a packet (`pkt-line.c` line 371).
pub(crate) const HUNG_UP: &str = "the remote end hung up unexpectedly";

/// The site's words for a service not served (the SSH site's `Unserved`), which a store not held here and a shard not
/// lent are too.
pub(crate) const NOT_SERVED: &str = "git-receive-pack is not served yet";

/// The words of the driver's refusal `failed` of the push to repository `name` (the module doc).
pub(crate) fn failed(failed: &PushFailed, name: &str) -> String {
  match failed {
    PushFailed::NotFound | PushFailed::NotGit => {
      format!("'{name}' does not appear to be a git repository")
    }
    PushFailed::NoStore => NOT_SERVED.to_owned(),
    PushFailed::Policy(_) => format!("{name}: the repository's policy cannot be read"),
    PushFailed::Exchange(refusal) => exchange_words(refusal),
    PushFailed::PastShare { past, share, held } => format!(
      "{name}: the push needs {} bytes where the repository's share of this server had {} left (its share {share} bytes, \
       {held} of them held) while other repositories push: retry once their pushes end",
      past.asked, past.left
    ),
    PushFailed::PastSnapshot { past, room } => format!(
      "{name}: the push needs {} bytes where its partition's snapshot had {} left of {room}: retry once the server lays \
       its snapshot out larger",
      past.asked, past.left
    ),
    PushFailed::Unchecked => format!("{name}: the push was answered before it was read whole"),
    PushFailed::Unlanded(why) => format!("{name}: {}: retry", unlanded_words(*why)),
  }
}

/// Where a push's homing and landing fell short ([`crate::git_push::Unlanded`]), in words a client's user can act on.
fn unlanded_words(why: crate::git_push::Unlanded) -> &'static str {
  use crate::git_push::Unlanded;
  match why {
    Unlanded::NamesNotPut => "the push's object names could not be put in the repository's store",
    Unlanded::HomingUntaken => "the push's objects were not carried to their homes in the region",
    Unlanded::PackNotHeld => "the push's pack was not held for its record",
    Unlanded::HomedShort => "the push's objects were not kept by enough homes in the region yet",
    Unlanded::NoLanding => "this node holds no changeset store for the repository",
    Unlanded::LandingUntaken => "the push's changeset was not carried to the node's store",
    Unlanded::StoreRefused => "the node's store refused the push's changeset",
  }
}

/// The words of an exchange the driver refused (the module doc): git's own where git dies with fixed ones.
fn exchange_words(refusal: &Refusal) -> String {
  match refusal {
    Refusal::PktLine(line) => pkt_line_words(line),
    Refusal::Truncated { .. } => HUNG_UP.to_owned(),
    Refusal::UnexpectedPacket { .. } => "protocol error: got an unexpected packet".to_owned(),
    Refusal::Command { .. } => "protocol error: expected old/new/ref".to_owned(),
    Refusal::Shallow { .. } => "protocol error: expected shallow sha".to_owned(),
    Refusal::ObjectFormat { .. } => "error: unsupported object format".to_owned(),
    Refusal::LineTooLong { .. } => "protocol error: impossibly long line".to_owned(),
    Refusal::Capabilities { .. } => {
      "protocol error: capabilities after the first command's".to_owned()
    }
    Refusal::PushCert { .. } => {
      "protocol error: a push certificate, which this server does not advertise".to_owned()
    }
    Refusal::Advertisement(_) => "the advertisement could not be written".to_owned(),
    Refusal::PastAdmission(past) => past_room(past),
    Refusal::AllocationRefused { bytes } => format!("out of memory: {bytes} bytes refused"),
  }
}

/// The words of a packet git's pkt-line reader refuses (`pkt-line.c` lines 371, 433, 450 and 458): git's length is the
/// packet's less its header where the packet is too long, and a length shorter than the header is 3, the one such
/// length that is no special packet.
fn pkt_line_words(refusal: &pktline::Refusal) -> String {
  match refusal {
    pktline::Refusal::NotHex { .. } => "protocol error: bad line length character".to_owned(),
    pktline::Refusal::ShorterThanHeader { .. } => format!(
      "protocol error: bad line length {}",
      HEADER_LEN.saturating_sub(1)
    ),
    pktline::Refusal::TooLong { len, .. } => format!(
      "protocol error: bad line length {}",
      len.saturating_sub(u64::try_from(HEADER_LEN).unwrap_or(u64::MAX))
    ),
    pktline::Refusal::Truncated { .. } => HUNG_UP.to_owned(),
    pktline::Refusal::PastAdmission(past) => past_room(past),
  }
}

/// The words of a push past its room (slates' own: git has no room).
fn past_room(past: &slates_git::PastAdmission) -> String {
  format!(
    "the push needs {} bytes where its room had {} left",
    past.asked, past.left
  )
}
