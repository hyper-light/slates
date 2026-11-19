//! `get-file` (§4.17.4 "4. Reads": `get-file` reads the node rows; item 11: a read streams from chunks; A-613 part 2):
//! a file's MD5, its properties with the entry properties, its ancestors' properties when asked, then its text, as
//! `svnserve`'s `get_file` writes them (`serve.c`, Subversion 1.15.0), held to stock `svnserve`'s transcripts
//! (`crates/svn/fixtures/reads`, sessions `get-file`, `hostile-get-file`, `svn-cat` and `svn-cat-big`):
//! - The parameters are `c(?r)bb?B`: the path, the revision (the youngest when absent), whether the properties and the
//!   text are wanted, and, optionally, whether the inherited properties are; any other shape ends the connection.
//! - After the authentication request the revision is opened (refused past the youngest), then the checksum taken: an
//!   absent path is "File not found", a directory "Attempted to get checksum of a *non*-file node" (160017,
//!   `svn_fs_fs__dag_file_checksum`). The checksum is the node row's MD5 (§4.17.4 item 2), the MD5 of no bytes for a
//!   file without a text, as `svn_fs_file_checksum` computes it when asked to force one.
//! - The answer is `( ( checksum ) rev ( props ) [ ( iprops ) ] )`: the property list is written even when not wanted,
//!   empty then; the inherited list only when asked, and empty when no ancestor has properties.
//! - When the text is wanted it follows as strings of [`STRING_BYTES`], `get_file`'s buffer, the last one shorter, then
//!   the empty string and a second response, `( success ( ) )`; or, when the green does not hold the text the index
//!   names, the empty string and a refusal, as `svnserve` answers a read that fails. The text is read from the green
//!   a piece at a time ([`super::Repository::text`]) and written a slice at a time ([`super::Stream`]), so no file is
//!   held whole: what one slice holds is the caller's room and one string.

use super::stream::{Progress, Stream};
use super::{
  Failure, Outcome, Reads, Repository, SUCCESS, boolean, owned_proplist, path_and_revision,
};
use crate::index::{ContentIdentity, NodeKind, Revision, Text};
use crate::ra_svn::{ItemError, Value, write};

/// `SVN_ERR_FS_NOT_FILE`.
/// Format: the filesystem category's start, 160,000, plus 17 (`svn_error_codes.h`); recorded (fixture `get-file`).
const SVN_ERR_FS_NOT_FILE: u32 = 160_017;

/// FSFS's refusal to take the checksum of a directory.
/// Format: `dag.c` `svn_fs_fs__dag_file_checksum`; recorded (fixture `get-file`).
const NOT_FILE: &str = "Attempted to get checksum of a *non*-file node";

/// The most bytes of a text one string carries.
/// Format: `get_file`'s buffer, `char buf[4096]` (`serve.c`), each full read written as one string; recorded (fixture
/// `get-file`: a 10,010-byte text as strings of 4,096, 4,096 and 1,818 bytes).
const STRING_BYTES: usize = 4_096;

/// The MD5 of no bytes: the checksum of a file without a text.
/// Format: RFC 1321 A.5, `MD5 ("") = d41d8cd98f00b204e9800998ecf8427e`; recorded (fixture `get-file`, the empty
/// text).
const EMPTY_MD5: [u8; 16] = [
  0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8, 0x42, 0x7e,
];

/// The digits a checksum is written in.
/// Format: `svn_checksum_to_cstring_display`, two lowercase hexadecimal digits a byte (`checksum.c`).
const HEX_DIGITS: &[u8] = b"0123456789abcdef";

/// The bits of a byte's high hexadecimal digit, shifted down.
/// Format: a hexadecimal digit is four bits.
const NIBBLE_BITS: u8 = 4;

/// The bits of a byte's low hexadecimal digit.
/// Format: a hexadecimal digit is four bits.
const LOW_NIBBLE: u8 = 0x0f;

/// A parsed `get-file` request.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileRequest {
  /// The file's path, the session's base joined with the given path.
  path: String,
  /// The revision, the youngest when absent.
  revision: Option<i64>,
  /// Whether its properties are wanted.
  properties: bool,
  /// Whether its text is wanted.
  contents: bool,
  /// Whether its ancestors' properties are wanted.
  inherited: bool,
}

impl FileRequest {
  /// `svn_ra_svn__parse_tuple(params, "c(?r)bb?B")`: a fifth item, when present, must be a boolean.
  fn parse(params: &[Value<'_>], base: &str) -> Option<Self> {
    let (path, revision) = path_and_revision(params, base)?;
    let inherited = match params.get(4) {
      None => false,
      some => boolean(some)?,
    };
    Some(Self {
      path,
      revision,
      properties: boolean(params.get(2))?,
      contents: boolean(params.get(3))?,
      inherited,
    })
  }
}

/// What `get-file` answers before the text.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileAnswer<'repository> {
  /// The revision.
  revision: Revision,
  /// The text's MD5.
  md5: [u8; 16],
  /// The file's properties with the entry properties; none when not wanted.
  properties: Vec<(Vec<u8>, Vec<u8>)>,
  /// Its ancestors' properties, when wanted.
  inherited: Option<super::OwnedAncestors<'repository>>,
  /// Its text.
  text: Option<Text>,
}

impl<'reads, R: Repository + ?Sized> Reads<'reads, R> {
  /// `get-file ( path [ rev ] want-props want-contents ? want-iprops )` (`serve.c` `get_file`): refused first where the
  /// session may not read the path (`must_have_access`); the head, then, when the text is wanted, a [`Stream`] of it.
  pub(super) fn file(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some(request) = FileRequest::parse(params, self.base) else {
      return Ok(Outcome::Close);
    };
    if let Some(outcome) = self.must_read(&request.path)? {
      return Ok(outcome);
    }
    let answer = match self.file_answer(&request) {
      Ok(answer) => answer,
      Err(failure) => return self.failure(&failure),
    };
    let digest = hex(&answer.md5);
    let mut items = vec![
      Value::List(vec![Value::String(&digest)]),
      Value::Number(answer.revision),
      owned_proplist(&answer.properties),
    ];
    if let Some(ancestors) = &answer.inherited {
      items.push(super::inherited_value(ancestors));
    }
    self.success(items)?;
    if !request.contents {
      return Ok(Outcome::Answered);
    }
    Ok(Outcome::Streaming(Stream::text(TextStream::new(
      answer.text,
    ))))
  }

  /// `get_file`'s work in its order: the revision, the checksum (the path must be a file), the properties.
  fn file_answer(&self, request: &FileRequest) -> Result<FileAnswer<'reads>, Failure> {
    let revision = self.at_or_youngest(request.revision)?;
    let node = self
      .node(&request.path, revision)?
      .ok_or_else(|| Failure::not_found(&request.path, revision))?;
    if node.kind() != NodeKind::File {
      return Err(Failure::new(SVN_ERR_FS_NOT_FILE, String::from(NOT_FILE)));
    }
    let properties = if request.properties {
      self.node_properties(&request.path, revision)?
    } else {
      Vec::new()
    };
    let inherited = if request.inherited {
      Some(self.owned_ancestors(&request.path, revision)?)
    } else {
      None
    };
    let text = node.text();
    Ok(FileAnswer {
      revision,
      md5: text.map_or(EMPTY_MD5, |text| text.md5),
      properties,
      inherited,
      text,
    })
  }
}

/// A file's text still to write: its identity and length, how much is written, and whether its end is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TextStream {
  /// The text's content identity; `None` for a file without one, whose text is empty.
  identity: Option<ContentIdentity>,
  /// Its length.
  size: u64,
  /// The bytes written so far.
  at: u64,
  /// Whether the empty string and the second response are written.
  ended: bool,
}

impl TextStream {
  /// A text to write from its start.
  fn new(text: Option<Text>) -> Self {
    Self {
      identity: text.map(|text| text.identity),
      size: text.map_or(0, |text| text.size),
      at: 0,
      ended: false,
    }
  }

  /// Writes strings of the text until `out` has grown by `room` bytes, at least one item, then the end.
  pub(super) fn resume<R: Repository + ?Sized>(
    &mut self,
    repository: &R,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<Progress, ItemError> {
    let start = out.len();
    while !self.ended {
      if self.at >= self.size {
        return self.finish(out, None);
      }
      let Some(piece) = self.next_string(repository) else {
        return self.finish(out, Some(&Failure::corrupt()));
      };
      write(&Value::String(&piece), out)?;
      let written = u64::try_from(piece.len()).unwrap_or(u64::MAX);
      self.at = self.at.saturating_add(written);
      if out.len().saturating_sub(start) >= room {
        return Ok(Progress::More);
      }
    }
    Ok(Progress::Done)
  }

  /// The next string: [`STRING_BYTES`] of the text, or what is left of it, gathered from the green's pieces; `None`
  /// when the green does not hold them.
  fn next_string<R: Repository + ?Sized>(&self, repository: &R) -> Option<Vec<u8>> {
    let left = self.size.saturating_sub(self.at);
    let want = usize::try_from(left).map_or(STRING_BYTES, |left| left.min(STRING_BYTES));
    let identity = self.identity.as_ref()?;
    let mut piece = Vec::with_capacity(want);
    while piece.len() < want {
      let offset = self.at.checked_add(u64::try_from(piece.len()).ok()?)?;
      let held = repository.text(identity, offset)?;
      let take = held.len().min(want.saturating_sub(piece.len()));
      if take == 0 {
        return None;
      }
      piece.extend_from_slice(held.get(..take)?);
    }
    Some(piece)
  }

  /// The text's end: the empty string, then the second response, `( success ( ) )`, or the refusal.
  fn finish(
    &mut self,
    out: &mut Vec<u8>,
    failure: Option<&Failure>,
  ) -> Result<Progress, ItemError> {
    self.ended = true;
    write(&Value::String(b""), out)?;
    match failure {
      None => write(
        &Value::List(vec![Value::Word(SUCCESS), Value::List(Vec::new())]),
        out,
      )?,
      Some(failure) => write(&failure.value(), out)?,
    }
    Ok(Progress::Done)
  }
}

/// A digest as `svn_checksum_to_cstring_display` writes it: two lowercase hexadecimal digits a byte.
fn hex(bytes: &[u8]) -> Vec<u8> {
  bytes
    .iter()
    .flat_map(|byte| [hex_digit(byte >> NIBBLE_BITS), hex_digit(byte & LOW_NIBBLE)])
    .collect()
}

/// One hexadecimal digit of a nibble.
fn hex_digit(nibble: u8) -> u8 {
  HEX_DIGITS.get(usize::from(nibble)).copied().unwrap_or(b'0')
}
