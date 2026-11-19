//! One report command's exchange on an ra_svn connection (§4.17.4 "4. Reads" and "5. The wire",
//! the reporter; A-616, its second part): the command and its report (part 1's [`Reporter`]), the
//! drive that answers it ([`Drive`]) written as editor commands ([`EditorWriter`]), the client's
//! answer to `close-edit` or `abort-edit`, and the command's last answer, as `svnserve` makes them
//! (`serve.c` `accept_report`; `libsvn_ra_svn/editorp.c` and `marshal.c`; Subversion's `1.14.x`
//! at `d434dc14`, 1.15.0 alike). It does no I/O: the caller hands it the client's bytes and writes
//! what it appends.
//!
//! What it holds to:
//! - **The drive under the caller's budget.** Each call writes calls until the output has grown by
//!   the room given or the drive's steps are spent, then hands the turn back ([`Turn::Wrote`]).
//! - **An early answer.** `editorp.c` looks for the client's answer before every editor command but
//!   `close-edit` and `abort-edit` (`check_for_error`): one there means the client gave up. The
//!   exchange then writes `abort-edit`, reads that answer, and fails the command: with "Successful
//!   edit status returned too soon" (210004) after a success, else with the client's own errors.
//! - **`close-edit`'s answer.** A success ends the command with `( success ( ) )`; a failure is
//!   answered `abort-edit`, then the client's errors written back as the command's failure
//!   (`ra_svn_close_edit`, `accept_report`'s `SVN_CMD_ERR`).
//! - **A drive's refusal.** The drive's `abort-edit` is written, the client's answer read, and the
//!   command fails with the drive's error, the client's errors after it when it answered one.
//! - **The client's errors** read as `svn_ra_svn__handle_failure_status` reads them: in order, a
//!   wrapping "command error" (210000) dropped, each message and file read as a C string; an empty
//!   list "Empty error list", an element that is no list or a list of wrappers alone "Malformed
//!   error list", an element of another shape "Malformed network data" (210004 each). An answer
//!   neither `success` nor `failure` is "Unknown status '…' in command response" (210004), and one
//!   of another shape "Malformed network data".
//!
//! **Where it differs from `svnserve`**, in when and what, never in what a stock client reads:
//! - it looks for an early answer before every command, where `svnserve` looks only after 4,096
//!   bytes written or a flush (`error_check_interval`, `svnserve.c` 801), so it notices one no
//!   later;
//! - an answer that is no item, or nested past three lists, ends the connection, where `svnserve`
//!   reads 64 levels and refuses the command;
//! - an error the client sent without a message is written back without one, where `svnserve`
//!   writes Subversion's own text for its number (`svn_strerror`).
//!
//! No I/O, no clock, no randomness, no panic.

#[cfg(test)]
mod tests;

use super::drive::{Call, Drive, DriveFailure, Guards, Next, Sources};
use super::editor::{EditorRefused, EditorWriter};
use super::tuple::{self, Field};
use super::{
  Begun, Context, LONGEST_WORD, PARSED_ITEM_BYTES, Progress, Refused, ReportFailure, Reporter,
  Request, SVN_ERR_RA_SVN_MALFORMED_DATA, write_empty_success,
};
use crate::handshake::ClientCapabilities;
use crate::ra_svn::{self, Bounds, Parsed, Parser, Value};
use crate::reads::Repository;

/// The deepest list a client's answer holds: `( failure ( ( apr-err message file line ) ) )`.
/// Format: `marshal.c` `svn_ra_svn__handle_failure_status`, three lists deep.
const ANSWER_DEPTH: usize = 3;

/// The shape a command's answer is read in: its status word and its parameters.
/// Format: `marshal.c` `svn_ra_svn__read_cmd_response`, `"wl"`.
const ANSWER_FORMAT: &[u8] = b"wl";

/// The shape of one error of a failure.
/// Format: `marshal.c` `svn_ra_svn__handle_failure_status`, `"nccn"`.
const ERROR_FORMAT: &[u8] = b"nccn";

/// The status word of a success.
/// Format: `marshal.c` `svn_ra_svn__read_cmd_response`.
const SUCCESS: &str = "success";

/// The status word of a failure.
/// Format: `marshal.c` `svn_ra_svn__read_cmd_response`.
const FAILURE: &str = "failure";

/// The start of a written failure, before its errors.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_failure`.
const FAILURE_START: &[u8] = b"( failure ( ";

/// The end of a written failure, after its errors.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_failure`.
const FAILURE_END: &[u8] = b") ) ";

/// The start of one written error.
/// Format: `marshal.c` `svn_ra_svn__write_tuple` of `"nccn"`.
const ERROR_START: &[u8] = b"( ";

/// The end of one written error.
/// Format: `marshal.c` `svn_ra_svn__write_tuple` of `"nccn"`.
const ERROR_END: &[u8] = b") ";

/// `SVN_ERR_RA_SVN_CMD_ERR`, the wrapper a failure's list may carry, dropped when read.
/// Format: the ra_svn category's start, 210,000, plus 0 (`svn_error_codes.h`).
const SVN_ERR_RA_SVN_CMD_ERR: u32 = 210_000;

/// The low 32 bits of an error's number, which `(apr_status_t)` keeps.
/// Format: `apr_status_t` is a 32-bit `int`.
const STATUS_BITS: u64 = 0xffff_ffff;

/// The message of an early answer that was a success.
/// Format: `editorp.c` `check_for_error_internal`.
const TOO_SOON: &str = "Successful edit status returned too soon";

/// The message of a failure without errors.
/// Format: `marshal.c` `svn_ra_svn__handle_failure_status`.
const EMPTY_ERROR_LIST: &str = "Empty error list";

/// The message of a failure whose errors are no lists, or wrappers alone.
/// Format: `marshal.c` `svn_ra_svn__handle_failure_status`.
const MALFORMED_ERROR_LIST: &str = "Malformed error list";

/// The message of an answer or an error of another shape.
/// Format: `marshal.c` `vparse_tuple`'s refusal.
const MALFORMED_DATA: &str = "Malformed network data";

/// What one call may do: the bytes it may add to the output and the steps it gives the drive. A
/// call writes at least one of the drive's calls and gives it at least one step, so an exchange
/// always ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
  /// The bytes the output may grow by before the call hands the turn back.
  pub room: usize,
  /// The steps the drive takes for each call it makes ([`Drive::next`]).
  pub steps: usize,
}

/// What [`Exchange::begin`] made of a command.
#[derive(Debug)]
pub enum Opened {
  /// The command was answered and its report is awaited.
  Reporting(Box<Exchange>),
  /// The command was refused, its failure written: the connection reads its next command.
  Failed(ReportFailure),
}

/// What one call of [`Exchange::receive`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
  /// The client's bytes end inside its next item: at least `needed` bytes from the input's start.
  NeedMore {
    /// How many bytes from the input's start the item needs at least.
    needed: usize,
  },
  /// The first `consumed` bytes of the input were taken: call again with the rest.
  Took {
    /// The bytes taken.
    consumed: usize,
  },
  /// The output grew by its room, or the drive's steps were spent: write the output, then call
  /// again with the same input.
  Wrote,
  /// The command is answered whole, `consumed` bytes of the input taken: the connection reads its
  /// next command.
  Answered {
    /// The bytes taken.
    consumed: usize,
  },
}

/// One error of a failure, as it is written: its number, message, file and line.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Error {
  /// The number, as written back.
  code: u64,
  /// The message.
  message: Vec<u8>,
  /// The source file, empty from a release build.
  file: Vec<u8>,
  /// The source line, 0 from a release build.
  line: u64,
}

impl Error {
  /// An error of slates' own, as a release build of `svnserve` writes one.
  fn own(code: u32, message: &str) -> Self {
    Self {
      code: u64::from(code),
      message: message.as_bytes().to_vec(),
      file: Vec::new(),
      line: 0,
    }
  }
}

/// What the client answered.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Answer {
  /// `( success ( ... ) )`.
  Success,
  /// A failure, or an answer read as one: its errors in order.
  Failure(Vec<Error>),
}

/// Why `abort-edit` was written.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Abort {
  /// The client answered before the drive ended.
  Early,
  /// The drive met a refusal.
  Refused(DriveFailure),
}

/// What the client's next item made.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Read {
  /// The item is not whole: at least `needed` bytes from the input's start.
  NeedMore(usize),
  /// The answer, and the bytes it took.
  Answered(Answer, usize),
}

/// Where the exchange stands.
#[derive(Debug)]
enum Phase {
  /// The report is being read.
  Reporting(Box<Reporter>),
  /// The drive is being written.
  Driving(Box<Drive>, EditorWriter),
  /// `close-edit` is written; the client's answer is next.
  Closing(EditorWriter),
  /// `abort-edit` is written; the client's answer is next.
  Aborting(Abort),
  /// The command is answered.
  Answered,
}

/// One report command's exchange (module documentation).
#[derive(Debug)]
pub struct Exchange {
  /// Where it stands.
  phase: Phase,
  /// What the client announced.
  capabilities: ClientCapabilities,
  /// The repository's UUID.
  uuid: Vec<u8>,
  /// The most bytes one of the client's items may take.
  item_bytes: usize,
  /// The most nodes the drive holds open at once.
  most_open: usize,
}

impl Exchange {
  /// Answers `request` on `context`'s session as `svnserve`'s command does, appending to `out`:
  /// the report is then awaited, or the command was refused and is over.
  ///
  /// # Errors
  /// [`Refused`] when an answer cannot be written, which none here can give.
  pub fn begin(
    context: &Context<'_>,
    request: Request,
    out: &mut Vec<u8>,
  ) -> Result<Opened, Refused> {
    Ok(match Reporter::begin(context, request, out)? {
      Begun::Reporting(reporter) => Opened::Reporting(Box::new(Self {
        phase: Phase::Reporting(Box::new(reporter)),
        capabilities: context.session.capabilities,
        uuid: context.session.uuid.clone(),
        item_bytes: context.item_bytes,
        most_open: context.most_open,
      })),
      Begun::Failed(failure) => Opened::Failed(failure),
    })
  }

  /// Goes on with `input`, the client's bytes not yet taken, appending to `out` within `budget`.
  ///
  /// # Errors
  /// [`Refused`] for client bytes that end the connection (an item past its bounds, a malformed
  /// report command, a report past its room), for a drive's call the writer refuses, and
  /// [`Refused::OutOfTurn`] after the command is answered.
  pub fn receive<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    sources: &Sources<'_, R, G>,
    input: &[u8],
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> Result<Turn, Refused> {
    match std::mem::replace(&mut self.phase, Phase::Answered) {
      Phase::Reporting(reporter) => self.report(reporter, input, out),
      Phase::Driving(drive, writer) => self.drive(drive, writer, sources, input, out, budget),
      Phase::Closing(writer) => self.closing(writer, input, out),
      Phase::Aborting(abort) => self.aborting(abort, input, out),
      Phase::Answered => Err(Refused::OutOfTurn),
    }
  }

  /// The report: part 1's reporter, its drive begun once it is finished.
  fn report(
    &mut self,
    mut reporter: Box<Reporter>,
    input: &[u8],
    out: &mut Vec<u8>,
  ) -> Result<Turn, Refused> {
    match reporter.receive(input, out)? {
      Progress::NeedMore { needed } => {
        self.phase = Phase::Reporting(reporter);
        Ok(Turn::NeedMore { needed })
      }
      Progress::Took { consumed } => {
        self.phase = Phase::Reporting(reporter);
        Ok(Turn::Took { consumed })
      }
      Progress::Finished { report, consumed } => {
        let drive = Drive::new(*report, &self.uuid);
        let writer = EditorWriter::new(&self.capabilities, self.most_open);
        self.phase = Phase::Driving(Box::new(drive), writer);
        Ok(Turn::Took { consumed })
      }
      Progress::Failed { consumed, .. } | Progress::Aborted { consumed } => {
        Ok(Turn::Answered { consumed })
      }
    }
  }

  /// The drive's calls written until the room or the steps are spent, an early answer met, or the
  /// edit closed or aborted.
  fn drive<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    mut drive: Box<Drive>,
    mut writer: EditorWriter,
    sources: &Sources<'_, R, G>,
    input: &[u8],
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> Result<Turn, Refused> {
    let start = out.len();
    let steps = budget.steps.max(1);
    loop {
      let next = drive.next(sources.index, sources.repository, sources.guards, steps);
      let call = match next {
        Next::Call(call) => call,
        Next::Paused => {
          self.phase = Phase::Driving(drive, writer);
          return Ok(Turn::Wrote);
        }
        Next::Ended => return Err(Refused::Editor(EditorRefused::OutOfOrder)),
      };
      if !input.is_empty() && self.looks_for_answer(&call) {
        write_call(&mut writer, &Call::AbortEdit, out)?;
        return self.aborting(Abort::Early, input, out);
      }
      write_call(&mut writer, &call, out)?;
      match call {
        Call::CloseEdit => return self.closing(writer, input, out),
        Call::AbortEdit => {
          let failure = drive
            .failure()
            .cloned()
            .ok_or(Refused::Editor(EditorRefused::OutOfOrder))?;
          return self.aborting(Abort::Refused(failure), input, out);
        }
        _ => {}
      }
      if out.len().saturating_sub(start) >= budget.room {
        self.phase = Phase::Driving(drive, writer);
        return Ok(Turn::Wrote);
      }
    }
  }

  /// Whether `editorp.c` looks for the client's answer before `call`: before every command but
  /// `close-edit` and `abort-edit`, and an absent entry the client is not sent.
  fn looks_for_answer(&self, call: &Call<'_>) -> bool {
    match call {
      Call::CloseEdit | Call::AbortEdit => false,
      Call::Absent { .. } => self.capabilities.absent_entries,
      _ => true,
    }
  }

  /// The client's answer to `close-edit`: a success ends the command; anything else is answered
  /// `abort-edit` and fails it with the client's errors.
  fn closing(
    &mut self,
    mut writer: EditorWriter,
    input: &[u8],
    out: &mut Vec<u8>,
  ) -> Result<Turn, Refused> {
    let (answer, consumed) = match self.read_answer(input)? {
      Read::Answered(answer, consumed) => (answer, consumed),
      Read::NeedMore(needed) => {
        self.phase = Phase::Closing(writer);
        return Ok(Turn::NeedMore { needed });
      }
    };
    match answer {
      Answer::Success => write_empty_success(out)?,
      Answer::Failure(errors) => {
        write_call(&mut writer, &Call::AbortEdit, out)?;
        write_failure(&errors, out)?;
      }
    }
    Ok(Turn::Answered { consumed })
  }

  /// The client's answer to `abort-edit`, then the command's failure: the early answer's or the
  /// drive's.
  fn aborting(&mut self, abort: Abort, input: &[u8], out: &mut Vec<u8>) -> Result<Turn, Refused> {
    let (answer, consumed) = match self.read_answer(input)? {
      Read::Answered(answer, consumed) => (answer, consumed),
      Read::NeedMore(needed) => {
        self.phase = Phase::Aborting(abort);
        return Ok(Turn::NeedMore { needed });
      }
    };
    let errors = match (abort, answer) {
      (Abort::Early, Answer::Success) => {
        vec![Error::own(SVN_ERR_RA_SVN_MALFORMED_DATA, TOO_SOON)]
      }
      (Abort::Early, Answer::Failure(errors)) => errors,
      (Abort::Refused(failure), answer) => {
        let mut errors = vec![Error::own(failure.code, &failure.message)];
        if let Answer::Failure(theirs) = answer {
          errors.extend(theirs);
        }
        errors
      }
    };
    write_failure(&errors, out)?;
    Ok(Turn::Answered { consumed })
  }

  /// The client's next item read as a command's answer, with the bytes it took.
  fn read_answer(&self, input: &[u8]) -> Result<Read, Refused> {
    let bounds = Bounds {
      item_bytes: self.item_bytes,
      word_bytes: LONGEST_WORD,
      depth: ANSWER_DEPTH,
      nodes: self.item_bytes.checked_div(PARSED_ITEM_BYTES).unwrap_or(0),
    };
    match Parser::new(bounds).parse(input).map_err(Refused::Item)? {
      Parsed::NeedMore { needed } => Ok(Read::NeedMore(needed)),
      Parsed::Item { value, consumed } => Ok(Read::Answered(answer_of(&value), consumed)),
    }
  }
}

/// A call written, a refusal of the writer's made the connection's end.
fn write_call(
  writer: &mut EditorWriter,
  call: &Call<'_>,
  out: &mut Vec<u8>,
) -> Result<(), Refused> {
  writer.write(call, out).map_err(Refused::Editor)
}

/// An item read as `svn_ra_svn__read_cmd_response` reads it.
fn answer_of(value: &Value<'_>) -> Answer {
  let malformed = || {
    Answer::Failure(vec![Error::own(
      SVN_ERR_RA_SVN_MALFORMED_DATA,
      MALFORMED_DATA,
    )])
  };
  let Value::List(items) = value else {
    return malformed();
  };
  let Ok(fields) = tuple::read(items, ANSWER_FORMAT) else {
    return malformed();
  };
  match fields.as_slice() {
    [Field::Word(Some(SUCCESS)), Field::List(Some(_))] => Answer::Success,
    [Field::Word(Some(FAILURE)), Field::List(Some(errors))] => Answer::Failure(errors_of(errors)),
    [Field::Word(Some(status)), Field::List(Some(_))] => Answer::Failure(vec![Error::own(
      SVN_ERR_RA_SVN_MALFORMED_DATA,
      &format!("Unknown status '{status}' in command response"),
    )]),
    _ => malformed(),
  }
}

/// A failure's errors as `svn_ra_svn__handle_failure_status` reads them: from the last back, the
/// first one it cannot read deciding the refusal, wrappers dropped.
fn errors_of(items: &[Value<'_>]) -> Vec<Error> {
  if items.is_empty() {
    return vec![Error::own(SVN_ERR_RA_SVN_MALFORMED_DATA, EMPTY_ERROR_LIST)];
  }
  let mut errors = Vec::new();
  for item in items.iter().rev() {
    match error_of(item) {
      Ok(Some(error)) => errors.push(error),
      Ok(None) => {}
      Err(refusal) => return vec![refusal],
    }
  }
  if errors.is_empty() {
    return vec![Error::own(
      SVN_ERR_RA_SVN_MALFORMED_DATA,
      MALFORMED_ERROR_LIST,
    )];
  }
  errors.reverse();
  errors
}

/// One error of a failure; none for a wrapper; the refusal of one of another shape.
fn error_of(item: &Value<'_>) -> Result<Option<Error>, Error> {
  let Value::List(fields) = item else {
    return Err(Error::own(
      SVN_ERR_RA_SVN_MALFORMED_DATA,
      MALFORMED_ERROR_LIST,
    ));
  };
  let malformed = || Error::own(SVN_ERR_RA_SVN_MALFORMED_DATA, MALFORMED_DATA);
  let fields = tuple::read(fields, ERROR_FORMAT).map_err(|_| malformed())?;
  let [
    Field::Number(Some(code)),
    Field::String(Some(message)),
    Field::String(Some(file)),
    Field::Number(Some(line)),
  ] = fields.as_slice()
  else {
    return Err(malformed());
  };
  let status = apr_status(*code);
  if status == u64::from(SVN_ERR_RA_SVN_CMD_ERR) {
    return Ok(None);
  }
  Ok(Some(Error {
    code: status,
    message: c_string(message).to_vec(),
    file: c_string(file).to_vec(),
    line: *line,
  }))
}

/// A number as `(apr_status_t)` keeps it and `(apr_uint64_t)` writes it back: its low 32 bits, read
/// as a signed `int` and widened with its sign.
fn apr_status(code: u64) -> u64 {
  let low = u32::try_from(code & STATUS_BITS).unwrap_or(0);
  let signed = i64::from(i32::from_ne_bytes(low.to_ne_bytes()));
  u64::from_ne_bytes(signed.to_ne_bytes())
}

/// The bytes before the first NUL, as C reads a string.
fn c_string(bytes: &[u8]) -> &[u8] {
  bytes.split(|byte| *byte == 0).next().unwrap_or_default()
}

/// A command's failure, `( failure ( ( apr-err message file line ) ... ) )`.
fn write_failure(errors: &[Error], out: &mut Vec<u8>) -> Result<(), Refused> {
  out.extend_from_slice(FAILURE_START);
  for error in errors {
    out.extend_from_slice(ERROR_START);
    leaf(&Value::Number(error.code), out)?;
    leaf(&Value::String(&error.message), out)?;
    leaf(&Value::String(&error.file), out)?;
    leaf(&Value::Number(error.line), out)?;
    out.extend_from_slice(ERROR_END);
  }
  out.extend_from_slice(FAILURE_END);
  Ok(())
}

/// One item appended.
fn leaf(value: &Value<'_>, out: &mut Vec<u8>) -> Result<(), Refused> {
  ra_svn::write(value, out).map_err(Refused::Item)
}
