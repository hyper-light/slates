//! What an `exec` request may name (A-633; §4.17.2 "Transports" item 2; RFC 4254 §6.5): the four commands the
//! version-control servers answer, each read in the quoting its own client writes, and nothing else. The server never
//! runs a command: "Normal precautions MUST be taken to prevent the execution of unauthorized commands" \[B: RFC 4254
//! §6.5\], and the surest precaution is to execute nothing, so each command is parsed into what it asks for.
//!
//! **git** sends `git-upload-pack '<path>'` or `git-receive-pack '<path>'`: the program, one space, and the path in
//! single quotes with each `'` and `!` written `'\''` and `'\!'` (`sq_quote_buf` in `quote.c` at `e1ec09b2`) \[C\].
//! [`parse`] takes the path back exactly as git's own `sq_dequote` does: a quoted run, then either the end or a
//! backslash before one of those two characters and a reopened quote; anything else is refused.
//!
//! **Mercurial** sends `hg -R <path> serve --stdio`, with ` --hidden` after it when the client asks for hidden
//! changesets, the path bare when it holds only `[a-zA-Z0-9@%_+=:,./-]` and otherwise single-quoted with `'\''` for a
//! quote (`_makeconnection` and `_serverquote` in `sshpeer.py`, Mercurial 7.2) \[C\]; Mercurial's own `contrib/hg-ssh`
//! accepts exactly those five words. The words are split as a shell would split those forms, and a byte a shell would
//! read as anything but a word (an unquoted `;`, `$`, `"` or backslash before anything but a quote) is refused.
//!
//! **Subversion** sends `svnserve -t` after the host: its tunnel agent's arguments are fixed (`find_tunnel_agent` in
//! `libsvn_ra_svn/client.c` at `d434dc14`) \[C\], and the repository is named later, inside ra_svn.

/// git's fetch program (gitprotocol-pack).
pub const GIT_UPLOAD_PACK: &str = "git-upload-pack";
/// git's push program.
pub const GIT_RECEIVE_PACK: &str = "git-receive-pack";
/// Format: the bytes of git's escape after a closing quote, a backslash, the character and the reopening quote
/// (`sq_dequote_step`).
const GIT_ESCAPE_LEN: usize = 3;
/// Subversion's tunnel command, whole (`client.c`).
pub const SVNSERVE_TUNNEL: &str = "svnserve -t";

/// A command the servers answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
  /// git's fetch, `git-upload-pack '<path>'`.
  GitUploadPack {
    /// The repository's path, unquoted.
    path: Vec<u8>,
  },
  /// git's push, `git-receive-pack '<path>'`.
  GitReceivePack {
    /// The repository's path, unquoted.
    path: Vec<u8>,
  },
  /// Mercurial's SSH protocol, `hg -R <path> serve --stdio`.
  Mercurial {
    /// The repository's path, unquoted.
    repository: Vec<u8>,
    /// Whether the client asked for hidden changesets (`--hidden`).
    hidden: bool,
  },
  /// Subversion's tunnel, `svnserve -t`.
  Subversion,
}

/// Why a command was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandRefusal {
  /// The command names no program the servers answer: a shell, `git-upload-archive`, anything else.
  UnknownProgram,
  /// The program's words are not the ones its client sends.
  Arguments,
  /// A quoted word is not closed, or holds a byte or an escape its client never writes; `at` is where.
  Quoting {
    /// The byte's offset in the command.
    at: u64,
  },
  /// The path is empty.
  EmptyPath,
}

/// The command `command` names, or why it is refused.
pub fn parse(command: &[u8]) -> Result<Command, CommandRefusal> {
  if command == SVNSERVE_TUNNEL.as_bytes() {
    return Ok(Command::Subversion);
  }
  if let Some(quoted) = after_program(command, GIT_UPLOAD_PACK) {
    let path = git_path(quoted, offset_of(command, quoted))?;
    return Ok(Command::GitUploadPack { path });
  }
  if let Some(quoted) = after_program(command, GIT_RECEIVE_PACK) {
    let path = git_path(quoted, offset_of(command, quoted))?;
    return Ok(Command::GitReceivePack { path });
  }
  if command.starts_with(b"hg ") {
    return mercurial(command);
  }
  Err(CommandRefusal::UnknownProgram)
}

/// What follows `program` and one space at the start of `command`.
fn after_program<'a>(command: &'a [u8], program: &str) -> Option<&'a [u8]> {
  command.strip_prefix(program.as_bytes())?.strip_prefix(b" ")
}

/// Where `part`, a tail of `command`, starts in it.
fn offset_of(command: &[u8], part: &[u8]) -> usize {
  command.len().saturating_sub(part.len())
}

/// A path git quoted with `sq_quote_buf`, read as `sq_dequote` reads it; `start` places it in the command.
fn git_path(quoted: &[u8], start: usize) -> Result<Vec<u8>, CommandRefusal> {
  let refused = |at: usize| CommandRefusal::Quoting {
    at: crate::to_u64(start.saturating_add(at)),
  };
  let mut path = Vec::with_capacity(quoted.len());
  let mut at = 0usize;
  if quoted.first() != Some(&b'\'') {
    return Err(refused(0));
  }
  loop {
    at = at.saturating_add(1);
    let Some(&byte) = quoted.get(at) else {
      return Err(refused(at));
    };
    if byte != b'\'' {
      path.push(byte);
      continue;
    }
    let after = quoted.get(at.saturating_add(1)..).unwrap_or(&[]);
    match after.get(..GIT_ESCAPE_LEN).unwrap_or(after) {
      [] => break,
      [b'\\', escaped @ (b'\'' | b'!'), b'\''] => {
        path.push(*escaped);
        at = at.saturating_add(GIT_ESCAPE_LEN);
      }
      _ => return Err(refused(at.saturating_add(1))),
    }
  }
  if path.is_empty() {
    return Err(CommandRefusal::EmptyPath);
  }
  Ok(path)
}

/// Mercurial's `hg -R <path> serve --stdio [--hidden]`.
fn mercurial(command: &[u8]) -> Result<Command, CommandRefusal> {
  let words = shell_words(command)?;
  let [program, flag, repository, rest @ ..] = words.as_slice() else {
    return Err(CommandRefusal::Arguments);
  };
  if program.as_slice() != b"hg" || flag.as_slice() != b"-R" {
    return Err(CommandRefusal::Arguments);
  }
  let hidden = match rest {
    [serve, stdio] if serve.as_slice() == b"serve" && stdio.as_slice() == b"--stdio" => false,
    [serve, stdio, hidden]
      if serve.as_slice() == b"serve"
        && stdio.as_slice() == b"--stdio"
        && hidden.as_slice() == b"--hidden" =>
    {
      true
    }
    _ => return Err(CommandRefusal::Arguments),
  };
  Ok(Command::Mercurial {
    repository: repository.clone(),
    hidden,
  })
}

/// Whether Mercurial leaves `byte` bare (`_serverquote`'s `[a-zA-Z0-9@%_+=:,./-]`).
fn is_bare(byte: u8) -> bool {
  byte.is_ascii_alphanumeric() || b"@%_+=:,./-".contains(&byte)
}

/// The words of `command` as a shell splits the forms Mercurial writes: bare runs, single-quoted runs and a
/// backslash-quoted `'`, words separated by single spaces; an empty word is refused, since Mercurial writes none.
fn shell_words(command: &[u8]) -> Result<Vec<Vec<u8>>, CommandRefusal> {
  let refused = |at: usize| CommandRefusal::Quoting {
    at: crate::to_u64(at),
  };
  let mut words = Vec::new();
  let mut word = Vec::new();
  let mut at = 0usize;
  while let Some(&byte) = command.get(at) {
    match byte {
      b' ' if word.is_empty() => return Err(refused(at)),
      b' ' => {
        words.push(core::mem::take(&mut word));
        at = at.saturating_add(1);
      }
      b'\'' => at = single_quoted(command, at, &mut word).ok_or(refused(at))?,
      b'\\' if command.get(at.saturating_add(1)) == Some(&b'\'') => {
        word.push(b'\'');
        at = at.saturating_add(2);
      }
      _ if is_bare(byte) => {
        word.push(byte);
        at = at.saturating_add(1);
      }
      _ => return Err(refused(at)),
    }
  }
  if word.is_empty() {
    return Err(refused(at));
  }
  words.push(word);
  Ok(words)
}

/// Appends the single-quoted run opening at `open` to `word`; the offset after its closing quote, or none if it never
/// closes.
fn single_quoted(command: &[u8], open: usize, word: &mut Vec<u8>) -> Option<usize> {
  let start = open.checked_add(1)?;
  let body = command.get(start..)?;
  let close = body.iter().position(|byte| *byte == b'\'')?;
  word.extend_from_slice(body.get(..close)?);
  start.checked_add(close)?.checked_add(1)
}
