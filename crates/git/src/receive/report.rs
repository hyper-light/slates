//! receive-pack's answer to a push (A-620 part 1; gitprotocol-pack "Report Status"; git's `report`, `report_v2`,
//! `report_message` and the end of `cmd_receive_pack`, v2.50.1).
//!
//! The report is `unpack ok` or `unpack <reason>`, then each command's `ok <ref>` or `ng <ref> <reason>` in the order
//! sent, then a flush. report-status-v2 adds, after an `ok`, the option lines of each report a hook made of it, each
//! report after the first repeating the `ok` line. Under side-band-64k the report goes out whole on band 1, after what
//! git sent on band 2 as it worked: a hook's words in the pieces git reads them in, and the `rp_error` lines (A-644).
//! The answer ends with a flush of its own.

use super::protected::HOOK_PIECE;
use super::{Declined, Outcome, Received, Refusal, Reported, unpack_words};
use crate::object_name::ObjectId;
use crate::out::Out;
use crate::pktline::{self, Packet};
use crate::sideband::{self, Band};

/// Format: `report_message`'s buffer, the most bytes an `rp_error` line takes, its LF included.
const MESSAGE_MAX: usize = 4096;
/// Format: what an `rp_error` line begins with.
const ERROR: &[u8] = b"error: ";
/// Format: what every report line ends with.
const LF: u8 = b'\n';
/// Format: the unpack status's word.
const UNPACK: &[u8] = b"unpack ";
/// Format: the unpack status of a pack read whole, or of none.
const OK: &[u8] = b"ok";
/// Format: a command applied.
const OK_REF: &[u8] = b"ok ";
/// Format: a command refused.
const NG_REF: &[u8] = b"ng ";
/// Format: an option line naming the ref written.
const OPTION_REFNAME: &[u8] = b"option refname ";
/// Format: an option line naming the id it was written from.
const OPTION_OLD: &[u8] = b"option old-oid ";
/// Format: an option line naming the id it was written to.
const OPTION_NEW: &[u8] = b"option new-oid ";
/// Format: the option line of a forced update.
const OPTION_FORCED: &[u8] = b"option forced-update";

/// The line git's `rp_error` sends for `text`: `error: ` and the text, cut to git's 4096 bytes with its LF.
pub fn message(text: &[u8]) -> Vec<u8> {
  let mut line = ERROR.to_vec();
  line.extend_from_slice(text);
  line.truncate(MESSAGE_MAX.saturating_sub(1));
  line.push(LF);
  line
}

/// Writes receive-pack's answer to `received`, after its advertisement: nothing for a request of no command; else each
/// refusal's band-2 line and the report as the client asked for them, and a last flush under side-band-64k. Refused
/// for a line past a pkt-line's room (git's "impossibly long line").
pub fn write_report(received: &Received, out: &mut impl Out) -> Result<(), Refusal> {
  let requested = &received.request.requested;
  if received.request.commands.is_empty() {
    return Ok(());
  }
  let multiplexed = requested.side_band_64k;
  if multiplexed {
    for outcome in &received.outcomes {
      if let Outcome::Ng(declined) = outcome {
        write_band_two(declined, out);
      }
    }
  }
  if requested.report_status || requested.report_status_v2 {
    let mut body = Vec::new();
    write_body(received, requested.report_status_v2, &mut body)?;
    if multiplexed {
      sideband::write(Band::Data, &body, out);
    } else {
      out.put(&body);
    }
  }
  if multiplexed {
    write_line(Packet::Flush, out)?;
  }
  Ok(())
}

/// What git sends on band 2 of a refused command: a hook's words in the pieces `copy_to_sideband` reads them in, then
/// the `rp_error` line.
fn write_band_two(declined: &Declined, out: &mut impl Out) {
  for piece in declined.hook.chunks(HOOK_PIECE) {
    sideband::write(Band::Progress, piece, out);
  }
  if let Some(line) = &declined.message {
    sideband::write(Band::Progress, line, out);
  }
}

/// The report's own packets: the unpack status, each command's line (and under v2 its option lines), a flush.
fn write_body(received: &Received, v2: bool, out: &mut impl Out) -> Result<(), Refusal> {
  let status = match &received.unpacked {
    Some(Err(refusal)) => unpack_words(refusal),
    _ => OK,
  };
  write_text(&[UNPACK, status], out)?;
  for (command, outcome) in received.request.commands.iter().zip(&received.outcomes) {
    let name = command.name.as_slice();
    match outcome {
      Outcome::Ng(declined) => write_text(&[NG_REF, name, b" ", &declined.reason], out)?,
      Outcome::Ok(reports) => {
        write_text(&[OK_REF, name], out)?;
        if v2 {
          write_reports(name, reports, out)?;
        }
      }
    }
  }
  write_line(Packet::Flush, out)
}

/// The option lines of each report a hook made of a command, each report after the first preceded by the command's
/// `ok` line again.
fn write_reports(name: &[u8], reports: &[Reported], out: &mut impl Out) -> Result<(), Refusal> {
  for (index, report) in reports.iter().enumerate() {
    if index > 0 {
      write_text(&[OK_REF, name], out)?;
    }
    if let Some(refname) = &report.refname {
      write_text(&[OPTION_REFNAME, refname], out)?;
    }
    for (word, id) in [(OPTION_OLD, report.old), (OPTION_NEW, report.new)] {
      if let Some(id) = id {
        write_text(&[word, hex(&id).as_slice()], out)?;
      }
    }
    if report.forced {
      write_text(&[OPTION_FORCED], out)?;
    }
  }
  Ok(())
}

/// Writes `parts` as one data packet ending with LF.
fn write_text(parts: &[&[u8]], out: &mut impl Out) -> Result<(), Refusal> {
  let mut line = parts.concat();
  line.push(LF);
  write_line(Packet::Data(&line), out)
}

/// `id` in lower-case hexadecimal, as git writes it.
fn hex(id: &ObjectId) -> Vec<u8> {
  id.to_string().into_bytes()
}

/// Writes `packet`, or refuses a data packet too long for one.
fn write_line(packet: Packet<'_>, out: &mut impl Out) -> Result<(), Refusal> {
  pktline::write(packet, out).map_err(|too_long| Refusal::LineTooLong { len: too_long.len })
}
