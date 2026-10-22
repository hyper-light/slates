//! The line protocol between the mount-route test on the host and the guest's `/init` (`docs/wip/virtiofs.md` §8
//! item 7). The host writes one [`Command`] a line to the guest's serial console; the guest answers each with one
//! [`Reply`] line, prefixed [`REPLY_PREFIX`] so the host can tell it from the kernel's own lines on the same console.
//!
//! Every parser here reads bytes a VM produced, so each refuses what it cannot read with a [`ProtocolError`] and never
//! allocates past a bound it is given: a byte run's length is checked against the caller's cap before anything is
//! built.

use std::fmt;

/// Format: the start of every reply line. The console also carries the kernel's lines, which have no such prefix.
pub const REPLY_PREFIX: &str = "PROBE ";
/// Shape: the longest command line the guest reads: a verb, a path under the mount, and three numbers.
pub const LONGEST_COMMAND: usize = 512;

/// Format: the arguments `store` takes: an offset, a length and a byte.
const STORE_ARGUMENTS: usize = 3;
/// Format: the arguments `wait` takes: an offset, a byte and a time.
const WAIT_ARGUMENTS: usize = 3;

/// One command from the host's test to the guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
  /// Open `path`, under the guest's mount of the tag, for reading and writing, and map its first `pages` pages
  /// shared, in place of any map made before. The path holds no whitespace.
  Map {
    /// The file, relative to the mount.
    path: String,
    /// How many pages to map.
    pages: u64,
  },
  /// Store `byte` into the map's bytes `[at, at + len)`, leaving the pages dirty.
  Store {
    /// The first byte.
    at: u64,
    /// How many bytes.
    len: u64,
    /// The value stored.
    byte: u8,
  },
  /// `msync` the whole map, so its dirty bytes are written back before the reply.
  Sync,
  /// The map's bytes `[at, at + len)`, as the guest's memory holds them.
  Read {
    /// The first byte.
    at: u64,
    /// How many bytes.
    len: u64,
  },
  /// Wait, touching memory only, until the map's byte at `at` holds `byte`, for at most `millis` milliseconds.
  Wait {
    /// The byte watched.
    at: u64,
    /// The value waited for.
    byte: u8,
    /// The longest wait.
    millis: u64,
  },
  /// Unmap, unmount, sync and power the VM off.
  Quit,
}

/// One reply from the guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
  /// The guest booted, took the console and mounted the tag.
  Ready,
  /// A map was made; `dax` says whether the kernel serves its file through the DAX window (`STATX_ATTR_DAX`).
  Mapped {
    /// Whether the file's inode is in DAX mode.
    dax: bool,
  },
  /// A store, a sync or the last step before power-off was done.
  Done,
  /// The bytes a read asked for.
  Bytes(Vec<u8>),
  /// A wait saw its byte after this many microseconds.
  Waited {
    /// Microseconds from the command to the byte.
    micros: u64,
  },
  /// A wait's time ran out with `seen` at the byte.
  TimedOut {
    /// The value the byte held at the end.
    seen: u8,
  },
  /// The command was refused: what was refused, and why.
  Refused(String),
  /// The guest cannot go on, and powers off after this line.
  Fatal(String),
}

/// A line the protocol does not speak.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolError {
  /// The line holds nothing.
  Empty,
  /// The line's first word is no verb the protocol has.
  UnknownVerb(String),
  /// The verb came with the wrong number of arguments.
  Arguments {
    /// The verb.
    verb: &'static str,
    /// The arguments it takes.
    wanted: usize,
    /// The arguments the line gave.
    got: usize,
  },
  /// A number did not parse, or did not fit its type.
  Number(String),
  /// A byte run is malformed, or the runs hold more bytes than the reader allows.
  Runs(String),
}

impl fmt::Display for ProtocolError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      ProtocolError::Empty => write!(f, "an empty line"),
      ProtocolError::UnknownVerb(verb) => write!(f, "no verb `{verb}`"),
      ProtocolError::Arguments { verb, wanted, got } => {
        write!(f, "`{verb}` takes {wanted} argument(s), not {got}")
      }
      ProtocolError::Number(word) => write!(f, "`{word}` is not a number of the right size"),
      ProtocolError::Runs(why) => write!(f, "byte runs: {why}"),
    }
  }
}

impl std::error::Error for ProtocolError {}

/// `word` as a number of type `T`.
fn number<T: std::str::FromStr>(word: &str) -> Result<T, ProtocolError> {
  word
    .parse()
    .map_err(|_| ProtocolError::Number(word.to_owned()))
}

/// The arguments after a verb, refused unless there are exactly `N` of them.
fn exactly<'w, const N: usize>(
  verb: &'static str,
  arguments: &[&'w str],
) -> Result<[&'w str; N], ProtocolError> {
  <[&str; N]>::try_from(arguments).map_err(|_| ProtocolError::Arguments {
    verb,
    wanted: N,
    got: arguments.len(),
  })
}

impl Command {
  /// The command's line, without its newline.
  pub fn encode(&self) -> String {
    match self {
      Command::Map { path, pages } => format!("map {path} {pages}"),
      Command::Store { at, len, byte } => format!("store {at} {len} {byte}"),
      Command::Sync => "sync".to_owned(),
      Command::Read { at, len } => format!("read {at} {len}"),
      Command::Wait { at, byte, millis } => format!("wait {at} {byte} {millis}"),
      Command::Quit => "quit".to_owned(),
    }
  }

  /// Parses one command line.
  ///
  /// # Errors
  /// [`ProtocolError`] when the line names no command, or a command with the wrong arguments.
  pub fn parse(line: &str) -> Result<Command, ProtocolError> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let Some((verb, arguments)) = words.split_first() else {
      return Err(ProtocolError::Empty);
    };
    match *verb {
      "map" => {
        let [path, pages] = exactly::<2>("map", arguments)?;
        Ok(Command::Map {
          path: path.to_owned(),
          pages: number(pages)?,
        })
      }
      "store" => {
        let [at, len, byte] = exactly::<STORE_ARGUMENTS>("store", arguments)?;
        Ok(Command::Store {
          at: number(at)?,
          len: number(len)?,
          byte: number(byte)?,
        })
      }
      "sync" => exactly::<0>("sync", arguments).map(|[]| Command::Sync),
      "read" => {
        let [at, len] = exactly::<2>("read", arguments)?;
        Ok(Command::Read {
          at: number(at)?,
          len: number(len)?,
        })
      }
      "wait" => {
        let [at, byte, millis] = exactly::<WAIT_ARGUMENTS>("wait", arguments)?;
        Ok(Command::Wait {
          at: number(at)?,
          byte: number(byte)?,
          millis: number(millis)?,
        })
      }
      "quit" => exactly::<0>("quit", arguments).map(|[]| Command::Quit),
      other => Err(ProtocolError::UnknownVerb(other.to_owned())),
    }
  }
}

impl Reply {
  /// The reply's line, with its [`REPLY_PREFIX`] and without its newline.
  pub fn encode(&self) -> String {
    let body = match self {
      Reply::Ready => "ready".to_owned(),
      Reply::Mapped { dax } => format!("mapped dax={}", u8::from(*dax)),
      Reply::Done => "done".to_owned(),
      Reply::Bytes(bytes) => format!("bytes {}", runs_of(bytes)),
      Reply::Waited { micros } => format!("waited {micros}"),
      Reply::TimedOut { seen } => format!("timedout {seen}"),
      Reply::Refused(why) => format!("refused {why}"),
      Reply::Fatal(why) => format!("fatal {why}"),
    };
    format!("{REPLY_PREFIX}{body}")
  }

  /// Parses one console line: `None` for a line that is not a reply (the kernel's own), else the reply, refused when
  /// malformed. A [`Reply::Bytes`] holding more than `most_bytes` bytes is refused before it is built.
  pub fn parse(line: &str, most_bytes: usize) -> Option<Result<Reply, ProtocolError>> {
    let body = line
      .trim_end_matches(['\r', '\n'])
      .strip_prefix(REPLY_PREFIX)?;
    let (verb, rest) = body.split_once(' ').unwrap_or((body, ""));
    Some(match verb {
      "ready" => Ok(Reply::Ready),
      "mapped" => match rest {
        "dax=1" => Ok(Reply::Mapped { dax: true }),
        "dax=0" => Ok(Reply::Mapped { dax: false }),
        other => Err(ProtocolError::Number(other.to_owned())),
      },
      "done" => Ok(Reply::Done),
      "bytes" => bytes_of(rest, most_bytes).map(Reply::Bytes),
      "waited" => number(rest).map(|micros| Reply::Waited { micros }),
      "timedout" => number(rest).map(|seen| Reply::TimedOut { seen }),
      "refused" => Ok(Reply::Refused(rest.to_owned())),
      "fatal" => Ok(Reply::Fatal(rest.to_owned())),
      other => Err(ProtocolError::UnknownVerb(other.to_owned())),
    })
  }
}

/// `bytes` as runs of one value: `value x count`, comma-separated (`104x4096,103x2048`). A map's pages are mostly whole
/// runs of one writer's byte, so a page reads back in a few characters.
fn runs_of(bytes: &[u8]) -> String {
  let mut runs: Vec<String> = Vec::new();
  let mut rest = bytes;
  while let Some(&value) = rest.first() {
    let count = rest.iter().take_while(|byte| **byte == value).count();
    runs.push(format!("{value}x{count}"));
    rest = rest.get(count..).unwrap_or_default();
  }
  runs.join(",")
}

/// The bytes `text`'s runs spell ([`runs_of`]), refused past `most_bytes` before any run is expanded.
fn bytes_of(text: &str, most_bytes: usize) -> Result<Vec<u8>, ProtocolError> {
  let mut runs = Vec::new();
  let mut total = 0usize;
  for run in text.split(',').filter(|run| !run.is_empty()) {
    let (value, count) = run
      .split_once('x')
      .ok_or_else(|| ProtocolError::Runs(format!("`{run}` is not value x count")))?;
    let value: u8 = number(value)?;
    let count: usize = number(count)?;
    total = total
      .checked_add(count)
      .filter(|total| *total <= most_bytes)
      .ok_or_else(|| ProtocolError::Runs(format!("more than the {most_bytes} bytes asked for")))?;
    runs.push((value, count));
  }
  let mut bytes = Vec::with_capacity(total);
  for (value, count) in runs {
    bytes.resize(bytes.len().saturating_add(count), value);
  }
  Ok(bytes)
}

#[cfg(test)]
mod tests {
  //! The protocol's round trips and its refusals of hostile lines (CLAUDE.md §4: every parser of external bytes).
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

  use super::*;

  /// Shape: a cap above every reply the round trips build.
  const ROOMY: usize = 1 << 20;

  /// Do: encode each command and parse it back. Expect the same command.
  #[test]
  fn every_command_reads_back_as_written() {
    for command in [
      Command::Map {
        path: "shared".to_owned(),
        pages: 8,
      },
      Command::Store {
        at: 4096,
        len: 2048,
        byte: 103,
      },
      Command::Sync,
      Command::Read { at: 0, len: 32768 },
      Command::Wait {
        at: 8192,
        byte: 72,
        millis: 10_000,
      },
      Command::Quit,
    ] {
      assert_eq!(Command::parse(&command.encode()), Ok(command));
    }
  }

  /// Do: encode each reply and parse it back, a run-length byte reply among them. Expect the same reply.
  #[test]
  fn every_reply_reads_back_as_written() {
    let mut mixed = vec![104u8; 4096];
    mixed.extend_from_slice(&[103u8; 2048]);
    mixed.push(0);
    for reply in [
      Reply::Ready,
      Reply::Mapped { dax: true },
      Reply::Mapped { dax: false },
      Reply::Done,
      Reply::Bytes(mixed),
      Reply::Bytes(Vec::new()),
      Reply::Waited { micros: 509 },
      Reply::TimedOut { seen: 104 },
      Reply::Refused("no map yet".to_owned()),
      Reply::Fatal("mount virtiofs slates: ENODEV".to_owned()),
    ] {
      assert_eq!(Reply::parse(&reply.encode(), ROOMY), Some(Ok(reply)));
    }
  }

  /// Do: parse the kernel's own console lines, and a reply line with the carriage return a raw console may add.
  /// Expect the kernel's lines to be no reply at all, and the reply to read past its line ending.
  #[test]
  fn the_kernels_lines_are_not_replies_and_a_line_ending_is_not_part_of_one() {
    assert_eq!(
      Reply::parse("[    0.025462] RDSEED32 is broken.", ROOMY),
      None
    );
    assert_eq!(Reply::parse("", ROOMY), None);
    assert_eq!(
      Reply::parse("PROBE waited 82\r", ROOMY),
      Some(Ok(Reply::Waited { micros: 82 }))
    );
  }

  /// The refusal a hostile byte-run line must get.
  #[derive(Clone, Copy, Debug)]
  enum Refusal {
    /// Malformed or too many bytes.
    Runs,
    /// A number that does not parse or fit.
    Number,
    /// Either: a count past `usize` may fail as a number or as a total, by platform.
    Either,
  }

  impl Refusal {
    fn covers(self, error: &ProtocolError) -> bool {
      matches!(
        (self, error),
        (Refusal::Runs | Refusal::Either, ProtocolError::Runs(_))
          | (Refusal::Number | Refusal::Either, ProtocolError::Number(_))
      )
    }
  }

  /// Do: parse byte runs that claim more bytes than the reader allows, past its cap and past `usize` itself, and runs
  /// that are malformed. Expect each refused, with its own error, before anything is built, so a hostile line cannot
  /// make the host allocate.
  #[test]
  fn byte_runs_past_the_readers_cap_are_refused_before_anything_is_built() {
    for (line, cap, expected) in [
      ("PROBE bytes 104x4097", 4096, Refusal::Runs),
      (
        "PROBE bytes 1x18446744073709551615,1x1",
        ROOMY,
        Refusal::Either,
      ),
      ("PROBE bytes 104x", ROOMY, Refusal::Number),
      ("PROBE bytes 256x1", ROOMY, Refusal::Number),
      ("PROBE bytes 104-1", ROOMY, Refusal::Runs),
    ] {
      match Reply::parse(line, cap) {
        Some(Err(error)) => assert!(expected.covers(&error), "{line}: refused, but as {error:?}"),
        other => panic!("{line}: not refused: {other:?}"),
      }
    }
  }

  /// Do: parse hostile command lines: empty, an unknown verb, too few and too many arguments, a number out of range.
  /// Expect each refused with its own error, never a command.
  #[test]
  fn hostile_command_lines_are_refused_by_name() {
    assert_eq!(Command::parse("   "), Err(ProtocolError::Empty));
    assert_eq!(
      Command::parse("format /mnt"),
      Err(ProtocolError::UnknownVerb("format".to_owned()))
    );
    assert_eq!(
      Command::parse("store 1 2"),
      Err(ProtocolError::Arguments {
        verb: "store",
        wanted: 3,
        got: 2
      })
    );
    assert_eq!(
      Command::parse("sync now"),
      Err(ProtocolError::Arguments {
        verb: "sync",
        wanted: 0,
        got: 1
      })
    );
    assert_eq!(
      Command::parse("store 0 1 256"),
      Err(ProtocolError::Number("256".to_owned()))
    );
    assert_eq!(
      Command::parse("read -1 4"),
      Err(ProtocolError::Number("-1".to_owned()))
    );
  }
}
