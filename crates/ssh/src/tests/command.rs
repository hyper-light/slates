//! What an `exec` may name (A-633): each client's own quoting, written here from its source, and every refusal.

use proptest::prelude::*;

use crate::command::{Command, CommandRefusal, parse};

/// git's `sq_quote_buf` (`quote.c` at `e1ec09b2`): the text in single quotes, each `'` and `!` written as `'\''` and
/// `'\!'`.
fn sq_quote(path: &[u8]) -> Vec<u8> {
  let mut quoted = vec![b'\''];
  for byte in path {
    if *byte == b'\'' || *byte == b'!' {
      quoted.extend_from_slice(b"'\\");
      quoted.push(*byte);
      quoted.push(b'\'');
    } else {
      quoted.push(*byte);
    }
  }
  quoted.push(b'\'');
  quoted
}

/// Mercurial 7.2's `_serverquote` (`sshpeer.py`): bare when every byte is in `[a-zA-Z0-9@%_+=:,./-]`, otherwise in
/// single quotes with `'` written `'\''`.
fn hg_quote(path: &[u8]) -> Vec<u8> {
  let bare = path
    .iter()
    .all(|byte| byte.is_ascii_alphanumeric() || b"@%_+=:,./-".contains(byte));
  if bare {
    return path.to_vec();
  }
  let mut quoted = vec![b'\''];
  for byte in path {
    if *byte == b'\'' {
      quoted.extend_from_slice(b"'\\''");
    } else {
      quoted.push(*byte);
    }
  }
  quoted.push(b'\'');
  quoted
}

/// `program`, a space and `argument`.
fn line(program: &[u8], argument: &[u8]) -> Vec<u8> {
  let mut command = program.to_vec();
  command.push(b' ');
  command.extend_from_slice(argument);
  command
}

/// A git command case: the program, the path, and the command the program names.
type CommandCase = (&'static [u8], &'static [u8], fn(Vec<u8>) -> Command);

/// A-633 (gitprotocol-pack over SSH; `connect.c` builds `prog SP sq_quote(path)`): do parse git's two programs with
/// paths git quoted, one holding a quote and an exclamation mark. Expect each path back exactly.
#[test]
fn git_commands_read_git_quoting() {
  let cases: [CommandCase; 3] = [
    (b"git-upload-pack", b"/srv/git/slates.git", |path| {
      Command::GitUploadPack { path }
    }),
    (b"git-receive-pack", b"team/it's done!.git", |path| {
      Command::GitReceivePack { path }
    }),
    (b"git-upload-pack", b"~ada/space dir/repo", |path| {
      Command::GitUploadPack { path }
    }),
  ];
  for (program, path, expected) in cases {
    assert_eq!(
      parse(&line(program, &sq_quote(path))),
      Ok(expected(path.to_vec()))
    );
  }
}

/// A-633 (`sshpeer.py`'s `hg -R %s serve --stdio%s`): do parse Mercurial's command with a bare path, a quoted one, one
/// holding a quote, and with `--hidden`. Expect the repository back and the flag read.
#[test]
fn mercurial_commands_read_mercurial_quoting() {
  let cases: [(&[u8], bool); 4] = [
    (b"/srv/hg/slates", false),
    (b"my repo", false),
    (b"it's", true),
    (b"a@b%c+d=e:f,g", false),
  ];
  for (repository, hidden) in cases {
    let mut command = b"hg -R ".to_vec();
    command.extend_from_slice(&hg_quote(repository));
    command.extend_from_slice(if hidden {
      b" serve --stdio --hidden"
    } else {
      b" serve --stdio"
    });
    let expected = Command::Mercurial {
      repository: repository.to_vec(),
      hidden,
    };
    assert_eq!(parse(&command), Ok(expected));
  }
}

/// A-633 (`client.c`'s tunnel agent: `svnserve -t`): do parse Subversion's tunnel command, and near misses. Expect the
/// tunnel, and the misses refused as programs not served.
#[test]
fn subversion_takes_its_tunnel_alone() {
  assert_eq!(parse(b"svnserve -t"), Ok(Command::Subversion));
  for miss in [
    &b"svnserve"[..],
    b"svnserve -t -r /",
    b"svnserve  -t",
    b"svnserve -T",
  ] {
    assert_eq!(
      parse(miss),
      Err(CommandRefusal::UnknownProgram),
      "{:?}",
      String::from_utf8_lossy(miss)
    );
  }
}

/// A-633 hostile: do parse git commands whose path is unquoted, unclosed, followed by more, escapes a byte git never
/// escapes, is empty, or follows two spaces; and programs no server answers. Expect each refused by its kind and place.
#[test]
fn bad_git_commands_are_refused() {
  let cases: [(&[u8], CommandRefusal); 9] = [
    (
      b"git-upload-pack /srv/repo",
      CommandRefusal::Quoting { at: 16 },
    ),
    (
      b"git-upload-pack '/srv/repo",
      CommandRefusal::Quoting { at: 26 },
    ),
    (
      b"git-upload-pack '/srv/repo'; rm -rf /",
      CommandRefusal::Quoting { at: 27 },
    ),
    (
      b"git-upload-pack 'a'\\x'b'",
      CommandRefusal::Quoting { at: 19 },
    ),
    (b"git-receive-pack ''", CommandRefusal::EmptyPath),
    (
      b"git-upload-pack  '/srv/repo'",
      CommandRefusal::Quoting { at: 16 },
    ),
    (
      b"git-upload-archive '/srv/repo'",
      CommandRefusal::UnknownProgram,
    ),
    (
      b"git upload-pack '/srv/repo'",
      CommandRefusal::UnknownProgram,
    ),
    (b"sh -c 'git-upload-pack x'", CommandRefusal::UnknownProgram),
  ];
  for (command, refusal) in cases {
    assert_eq!(
      parse(command),
      Err(refusal),
      "{:?}",
      String::from_utf8_lossy(command)
    );
  }
}

/// A-633 hostile: do parse Mercurial commands with shell metacharacters, double quotes, a backslash before anything but
/// a quote, a doubled space, a missing or an extra word. Expect each refused by its kind.
#[test]
fn bad_mercurial_commands_are_refused() {
  let cases: [(&[u8], CommandRefusal); 8] = [
    (
      b"hg -R repo serve --stdio; id",
      CommandRefusal::Quoting { at: 24 },
    ),
    (
      b"hg -R $HOME serve --stdio",
      CommandRefusal::Quoting { at: 6 },
    ),
    (
      b"hg -R \"repo\" serve --stdio",
      CommandRefusal::Quoting { at: 6 },
    ),
    (
      b"hg -R re\\po serve --stdio",
      CommandRefusal::Quoting { at: 8 },
    ),
    (
      b"hg -R  repo serve --stdio",
      CommandRefusal::Quoting { at: 6 },
    ),
    (b"hg -R repo serve", CommandRefusal::Arguments),
    (
      b"hg -R repo serve --stdio --debugger",
      CommandRefusal::Arguments,
    ),
    (b"hg serve --stdio", CommandRefusal::Arguments),
  ];
  for (command, refusal) in cases {
    assert_eq!(
      parse(command),
      Err(refusal),
      "{:?}",
      String::from_utf8_lossy(command)
    );
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-633: do quote any non-empty path as git quotes it and as Mercurial quotes it, then parse the commands. Expect the
  /// path back from both.
  #[test]
  fn any_path_quoted_by_its_client_reads_back(path in proptest::collection::vec(any::<u8>(), 1..80)) {
    let git = parse(&line(b"git-upload-pack", &sq_quote(&path)));
    prop_assert_eq!(git, Ok(Command::GitUploadPack { path: path.clone() }));
    let mut hg = b"hg -R ".to_vec();
    hg.extend_from_slice(&hg_quote(&path));
    hg.extend_from_slice(b" serve --stdio");
    prop_assert_eq!(parse(&hg), Ok(Command::Mercurial { repository: path, hidden: false }));
  }

  /// A-633 hostile: do parse any bytes as a command. Expect a command or a typed refusal, never a panic.
  #[test]
  fn any_command_parses_or_refuses(command in proptest::collection::vec(any::<u8>(), 0..120)) {
    let outcome = parse(&command);
    prop_assert!(outcome.is_ok() || outcome.is_err());
  }
}
