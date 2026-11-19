//! A drive's calls written as ra_svn's editor commands (§4.17.4 "5. The wire", the reporter's
//! drive; A-616, its second part): byte for byte as `svnserve` writes them through
//! `libsvn_ra_svn/editorp.c`'s editor and `marshal.c`'s `svn_ra_svn__write_cmd_*` (Subversion's
//! `1.14.x` at `d434dc14`; 1.15.0's `editorp.c` alike).
//!
//! What the writer holds to:
//! - **Tokens.** Every opened node gets a token, `d` for a directory and `c` for a file, then a
//!   number from one counter that starts at 0 and counts every open and add in order
//!   (`make_token`). A command on a node names its token; one on an entry names its directory's.
//! - **Absent entries** are written only to a client that announced `absent-entries`; to any other
//!   nothing is written, as `ra_svn_absent_dir` and `ra_svn_absent_file` skip them.
//! - **A text delta** is `apply-textdelta`, then the svndiff stream as `textdelta-chunk`s, then
//!   `textdelta-end`. The stream's four-byte header is one chunk, written at its first window or,
//!   with none, at its end (`svndiff.c` `window_handler` writes it on its first call, the last
//!   one's empty window included). Each window is up to three chunks, as `window_handler`'s three
//!   stream writes make them: its integers, then its instruction section and its new-data section,
//!   each when it is not empty.
//! - **The svndiff version** is 2, each section LZ4 when that is shorter, to a client that accepts
//!   it; 0 to any other (`svn_ra_svn__svndiff_version`). Never 1, which is zlib: slates sends no
//!   zlib, and every client takes 0 (A-500).
//! - **Order.** A call the editor's order does not allow where it comes is refused, typed, with
//!   nothing written ([`EditorRefused::OutOfOrder`]): the drive's own mistake, never a panic.
//!
//! One difference, in bytes alone: an LZ4 block is `lz4_flex`'s, where `svnserve`'s is liblz4's, so
//! a compressed section may differ while it decodes to the same window. Every section stored
//! uncompressed is `svnserve`'s byte for byte.
//!
//! No I/O, no clock, no randomness, no panic.

#[cfg(test)]
mod tests;

use super::drive::Call;
use crate::handshake::ClientCapabilities;
use crate::index::{NodeKind, Revision};
use crate::ra_svn::{self, ItemError, Value};
use crate::svndiff::{Compression, Encoder, INTEGER_LIMIT, SvndiffError, Version, Window};

/// The first letter of a directory's token.
/// Format: `editorp.c`'s `make_token('d', ...)` in `ra_svn_open_root`, `ra_svn_add_dir` and
/// `ra_svn_open_dir`.
const DIRECTORY_TOKEN: char = 'd';

/// The first letter of a file's token.
/// Format: `editorp.c`'s `make_token('c', ...)` in `ra_svn_add_file` and `ra_svn_open_file`.
const FILE_TOKEN: char = 'c';

/// The start of `target-rev`, before its revision.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_target_rev`.
const TARGET_REV: &[u8] = b"( target-rev ( ";

/// The start of `open-root`, before its revision's list.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_open_root`.
const OPEN_ROOT: &[u8] = b"( open-root ( ";

/// The start of `delete-entry`, before its path.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_delete_entry`.
const DELETE_ENTRY: &[u8] = b"( delete-entry ( ";

/// The start of `add-dir`, before its path.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_add_dir`.
const ADD_DIR: &[u8] = b"( add-dir ( ";

/// The start of `open-dir`, before its path.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_open_dir`.
const OPEN_DIR: &[u8] = b"( open-dir ( ";

/// The start of `change-dir-prop`, before its token.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_change_dir_prop`.
const CHANGE_DIR_PROP: &[u8] = b"( change-dir-prop ( ";

/// The start of `close-dir`, before its token.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_close_dir`.
const CLOSE_DIR: &[u8] = b"( close-dir ( ";

/// The start of `absent-dir`, before its path.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_absent_dir`.
const ABSENT_DIR: &[u8] = b"( absent-dir ( ";

/// The start of `add-file`, before its path.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_add_file`.
const ADD_FILE: &[u8] = b"( add-file ( ";

/// The start of `open-file`, before its path.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_open_file`.
const OPEN_FILE: &[u8] = b"( open-file ( ";

/// The start of `change-file-prop`, before its token.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_change_file_prop`.
const CHANGE_FILE_PROP: &[u8] = b"( change-file-prop ( ";

/// The start of `close-file`, before its token.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_close_file`.
const CLOSE_FILE: &[u8] = b"( close-file ( ";

/// The start of `absent-file`, before its path.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_absent_file`.
const ABSENT_FILE: &[u8] = b"( absent-file ( ";

/// The start of `textdelta-chunk`, before its token.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_textdelta_chunk`.
const TEXTDELTA_CHUNK: &[u8] = b"( textdelta-chunk ( ";

/// The start of `textdelta-end`, before its token.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_textdelta_end`.
const TEXTDELTA_END: &[u8] = b"( textdelta-end ( ";

/// The start of `apply-textdelta`, before its token.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_apply_textdelta`.
const APPLY_TEXTDELTA: &[u8] = b"( apply-textdelta ( ";

/// `close-edit`, whole.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_close_edit`.
const CLOSE_EDIT: &[u8] = b"( close-edit ( ) ) ";

/// `abort-edit`, whole.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_abort_edit`.
const ABORT_EDIT: &[u8] = b"( abort-edit ( ) ) ";

/// The start of a list inside a command.
/// Format: `marshal.c` `svn_ra_svn__start_list`.
const START_LIST: &[u8] = b"( ";

/// The end of a list inside a command.
/// Format: `marshal.c` `svn_ra_svn__end_list`.
const END_LIST: &[u8] = b") ";

/// The end of a command: its parameters' list and the command's own.
/// Format: `marshal.c`'s `writebuf_write_literal(conn, pool, ") ) ")` closing every command.
const END_COMMAND: &[u8] = b") ) ";

/// The digits a digest is written in, lowercase.
/// Format: `svn_checksum_to_cstring`, through `svn__digest_to_cstring`'s lowercase table.
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// The bits of one hexadecimal digit, by which a byte's high digit is shifted down.
/// Format: a byte is two hexadecimal digits of four bits.
const NIBBLE_BITS: u32 = 4;

/// The mask of a byte's low hexadecimal digit.
/// Format: a byte is two hexadecimal digits of four bits.
const NIBBLE_MASK: u8 = 0x0f;

/// The bit an svndiff integer's byte carries when another byte follows it.
/// Format: `notes/svndiff`: "the high bit of each byte is set on all bytes but the last".
const CONTINUATION_BIT: u8 = 0x80;

/// The bits of an svndiff integer one byte carries.
/// Format: `notes/svndiff`: seven bits a byte, the most significant group first.
const GROUP_BITS: u32 = 7;

/// The integers that begin an svndiff window: source view offset and length, target view length,
/// instruction section length, new-data section length.
/// Format: `notes/svndiff` 37-43, the window header.
pub(super) const WINDOW_HEADER_INTEGERS: usize = 5;

/// Why a call could not be written; nothing was appended to the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorRefused {
  /// A call the editor's order does not allow where it came: a node call with no directory open,
  /// a property change inside a delta, a window outside one, a close of the wrong kind, anything
  /// after the edit ended.
  OutOfOrder,
  /// A node opened past the most nodes the writer holds open at once.
  TooDeep,
  /// A window the svndiff encoder refuses.
  Window(SvndiffError),
  /// An item that could not be written.
  Item(ItemError),
}

/// Where the edit stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
  /// Nothing written yet: the target revision or the root may come.
  Begun,
  /// The target revision written: the root comes next.
  Targeted,
  /// The root opened.
  Rooted,
  /// `close-edit` written: only an `abort-edit` may follow, when the client refuses the edit
  /// (`ra_svn_close_edit` writes one after a failure answers it).
  Closed,
  /// The edit aborted, or closed and answered: nothing more comes.
  Ended,
}

/// A node open now and the token that names it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OpenNode {
  /// A directory or a file.
  kind: NodeKind,
  /// Its token, `d` or `c` and its number.
  token: String,
}

/// The text delta being written.
#[derive(Clone, Debug)]
struct Delta {
  /// The svndiff encoder, which holds the previous window's source view.
  encoder: Encoder,
  /// Whether the stream's header chunk is written.
  header_written: bool,
}

/// Writes one drive's calls as ra_svn's editor commands, holding the tokens of the nodes open.
#[derive(Clone, Debug)]
pub struct EditorWriter {
  /// The svndiff version the client takes.
  version: Version,
  /// How a version-2 window's sections are stored.
  compression: Compression,
  /// Whether the client takes `absent-dir` and `absent-file`.
  absent_entries: bool,
  /// The most nodes open at once, the caller's bound.
  most_open: usize,
  /// The next token's number.
  next_token: u64,
  /// The nodes open now, the innermost last.
  open: Vec<OpenNode>,
  /// The text delta being written, if any.
  delta: Option<Delta>,
  /// Where the edit stands.
  phase: Phase,
  /// One window's encoding, kept between windows.
  window_bytes: Vec<u8>,
}

impl EditorWriter {
  /// A writer for a client that announced `capabilities`, holding at most `most_open` nodes open at
  /// once (the caller's bound: the depth its drive reaches).
  pub fn new(capabilities: &ClientCapabilities, most_open: usize) -> Self {
    let (version, compression) = if capabilities.accepts_svndiff2 {
      (Version::Two, Compression::Lz4)
    } else {
      (Version::Zero, Compression::Stored)
    };
    Self {
      version,
      compression,
      absent_entries: capabilities.absent_entries,
      most_open,
      next_token: 0,
      open: Vec::new(),
      delta: None,
      phase: Phase::Begun,
      window_bytes: Vec::new(),
    }
  }

  /// Appends `call`'s commands to `out`: none, one, or for a window up to four.
  ///
  /// # Errors
  /// [`EditorRefused`]; nothing is appended then, and the writer is as it was.
  pub fn write(&mut self, call: &Call<'_>, out: &mut Vec<u8>) -> Result<(), EditorRefused> {
    let start = out.len();
    let written = self.dispatch(call, out);
    if written.is_err() {
      out.truncate(start);
    }
    written
  }

  /// Writes one call; each arm checks the order before it writes or changes anything.
  fn dispatch(&mut self, call: &Call<'_>, out: &mut Vec<u8>) -> Result<(), EditorRefused> {
    match *call {
      Call::TargetRevision(revision) => self.target_revision(revision, out),
      Call::OpenRoot { base, .. } => self.open_root(base, out),
      Call::Open {
        kind, path, base, ..
      } => self.open_entry(kind, path, base, out),
      Call::Add {
        kind,
        path,
        copy_from,
        ..
      } => self.add_entry(kind, path, copy_from, out),
      Call::Delete { path, revision } => self.delete_entry(path, revision, out),
      Call::Absent { kind, path } => self.absent_entry(kind, path, out),
      Call::ChangeProperty { name, value } => self.change_property(name, value, out),
      Call::ApplyTextDelta { base_md5 } => self.apply_text_delta(base_md5, out),
      Call::Window(window) => self.window(window, out),
      Call::TextDeltaEnd => self.text_delta_end(out),
      Call::CloseFile { md5 } => self.close_file(&md5, out),
      Call::CloseDirectory => self.close_directory(out),
      Call::CloseEdit => self.close_edit(out),
      Call::AbortEdit => self.abort_edit(out),
    }
  }

  /// `target-rev`: before anything else.
  fn target_revision(
    &mut self,
    revision: Revision,
    out: &mut Vec<u8>,
  ) -> Result<(), EditorRefused> {
    if self.phase != Phase::Begun {
      return Err(EditorRefused::OutOfOrder);
    }
    out.extend_from_slice(TARGET_REV);
    number(revision, out)?;
    out.extend_from_slice(END_COMMAND);
    self.phase = Phase::Targeted;
    Ok(())
  }

  /// `open-root`: once, before any node.
  fn open_root(&mut self, base: Option<Revision>, out: &mut Vec<u8>) -> Result<(), EditorRefused> {
    if !matches!(self.phase, Phase::Begun | Phase::Targeted) {
      return Err(EditorRefused::OutOfOrder);
    }
    let root = self.next_node(NodeKind::Directory)?;
    out.extend_from_slice(OPEN_ROOT);
    optional_revision(base, out)?;
    string(root.token.as_bytes(), out)?;
    out.extend_from_slice(END_COMMAND);
    self.push(root);
    self.phase = Phase::Rooted;
    Ok(())
  }

  /// `open-dir` or `open-file`, inside the innermost open directory.
  fn open_entry(
    &mut self,
    kind: NodeKind,
    path: &str,
    base: Option<Revision>,
    out: &mut Vec<u8>,
  ) -> Result<(), EditorRefused> {
    let parent = self.directory()?;
    let child = self.next_node(kind)?;
    out.extend_from_slice(match kind {
      NodeKind::Directory => OPEN_DIR,
      NodeKind::File => OPEN_FILE,
    });
    string(path.as_bytes(), out)?;
    string(parent.as_bytes(), out)?;
    string(child.token.as_bytes(), out)?;
    optional_revision(base, out)?;
    out.extend_from_slice(END_COMMAND);
    self.push(child);
    Ok(())
  }

  /// `add-dir` or `add-file`, inside the innermost open directory, with its copy source if any.
  fn add_entry(
    &mut self,
    kind: NodeKind,
    path: &str,
    copy_from: Option<(&str, Revision)>,
    out: &mut Vec<u8>,
  ) -> Result<(), EditorRefused> {
    let parent = self.directory()?;
    let child = self.next_node(kind)?;
    out.extend_from_slice(match kind {
      NodeKind::Directory => ADD_DIR,
      NodeKind::File => ADD_FILE,
    });
    string(path.as_bytes(), out)?;
    string(parent.as_bytes(), out)?;
    string(child.token.as_bytes(), out)?;
    out.extend_from_slice(START_LIST);
    if let Some((source, revision)) = copy_from {
      string(source.as_bytes(), out)?;
      number(revision, out)?;
    }
    out.extend_from_slice(END_LIST);
    out.extend_from_slice(END_COMMAND);
    self.push(child);
    Ok(())
  }

  /// `delete-entry`, inside the innermost open directory.
  fn delete_entry(
    &self,
    path: &str,
    revision: Option<Revision>,
    out: &mut Vec<u8>,
  ) -> Result<(), EditorRefused> {
    let parent = self.directory()?;
    out.extend_from_slice(DELETE_ENTRY);
    string(path.as_bytes(), out)?;
    optional_revision(revision, out)?;
    string(parent.as_bytes(), out)?;
    out.extend_from_slice(END_COMMAND);
    Ok(())
  }

  /// `absent-dir` or `absent-file`, inside the innermost open directory; nothing to a client that
  /// did not announce `absent-entries`.
  fn absent_entry(
    &self,
    kind: NodeKind,
    path: &str,
    out: &mut Vec<u8>,
  ) -> Result<(), EditorRefused> {
    let parent = self.directory()?;
    if !self.absent_entries {
      return Ok(());
    }
    out.extend_from_slice(match kind {
      NodeKind::Directory => ABSENT_DIR,
      NodeKind::File => ABSENT_FILE,
    });
    string(path.as_bytes(), out)?;
    string(parent.as_bytes(), out)?;
    out.extend_from_slice(END_COMMAND);
    Ok(())
  }

  /// `change-dir-prop` or `change-file-prop` on the innermost open node, outside a delta.
  fn change_property(
    &self,
    name: &str,
    value: Option<&[u8]>,
    out: &mut Vec<u8>,
  ) -> Result<(), EditorRefused> {
    let node = self.innermost()?;
    out.extend_from_slice(match node.kind {
      NodeKind::Directory => CHANGE_DIR_PROP,
      NodeKind::File => CHANGE_FILE_PROP,
    });
    string(node.token.as_bytes(), out)?;
    string(name.as_bytes(), out)?;
    out.extend_from_slice(START_LIST);
    if let Some(value) = value {
      string(value, out)?;
    }
    out.extend_from_slice(END_LIST);
    out.extend_from_slice(END_COMMAND);
    Ok(())
  }

  /// `apply-textdelta` on the innermost open node, a file, with the base text's MD5 if any.
  fn apply_text_delta(
    &mut self,
    base_md5: Option<[u8; 16]>,
    out: &mut Vec<u8>,
  ) -> Result<(), EditorRefused> {
    let file = self.file()?;
    out.extend_from_slice(APPLY_TEXTDELTA);
    string(file.as_bytes(), out)?;
    out.extend_from_slice(START_LIST);
    if let Some(digest) = base_md5 {
      string(&hex(&digest), out)?;
    }
    out.extend_from_slice(END_LIST);
    out.extend_from_slice(END_COMMAND);
    self.delta = Some(Delta {
      encoder: Encoder::new(self.version),
      header_written: false,
    });
    Ok(())
  }

  /// One window of the open delta: the stream's header first if it is not out, then the window's
  /// sections as `window_handler` writes them.
  fn window(&mut self, window: &Window, out: &mut Vec<u8>) -> Result<(), EditorRefused> {
    let (Phase::Rooted, Some(file), Some(delta)) =
      (self.phase, self.open.last(), self.delta.as_mut())
    else {
      return Err(EditorRefused::OutOfOrder);
    };
    if file.kind != NodeKind::File {
      return Err(EditorRefused::OutOfOrder);
    }
    let mut encoder = delta.encoder.clone();
    self.window_bytes.clear();
    encoder
      .window(window, self.compression, &mut self.window_bytes)
      .map_err(EditorRefused::Window)?;
    let sections = window_sections(&self.window_bytes)?;
    if !delta.header_written {
      chunk(&file.token, &encoder.header(), out)?;
    }
    for section in sections.into_iter().filter(|section| !section.is_empty()) {
      chunk(&file.token, section, out)?;
    }
    delta.encoder = encoder;
    delta.header_written = true;
    Ok(())
  }

  /// The end of the open delta: the stream's header if no window brought it, then `textdelta-end`.
  fn text_delta_end(&mut self, out: &mut Vec<u8>) -> Result<(), EditorRefused> {
    let (Phase::Rooted, Some(file), Some(delta)) = (self.phase, self.open.last(), &self.delta)
    else {
      return Err(EditorRefused::OutOfOrder);
    };
    if file.kind != NodeKind::File {
      return Err(EditorRefused::OutOfOrder);
    }
    if !delta.header_written {
      chunk(&file.token, &delta.encoder.header(), out)?;
    }
    out.extend_from_slice(TEXTDELTA_END);
    string(file.token.as_bytes(), out)?;
    out.extend_from_slice(END_COMMAND);
    self.delta = None;
    Ok(())
  }

  /// `close-file` on the innermost open node, a file, with its text's MD5.
  fn close_file(&mut self, md5: &[u8; 16], out: &mut Vec<u8>) -> Result<(), EditorRefused> {
    let file = self.file()?;
    out.extend_from_slice(CLOSE_FILE);
    string(file.as_bytes(), out)?;
    out.extend_from_slice(START_LIST);
    string(&hex(md5), out)?;
    out.extend_from_slice(END_LIST);
    out.extend_from_slice(END_COMMAND);
    self.open.pop();
    Ok(())
  }

  /// `close-dir` on the innermost open node, a directory.
  fn close_directory(&mut self, out: &mut Vec<u8>) -> Result<(), EditorRefused> {
    let directory = self.directory()?;
    out.extend_from_slice(CLOSE_DIR);
    string(directory.as_bytes(), out)?;
    out.extend_from_slice(END_COMMAND);
    self.open.pop();
    Ok(())
  }

  /// `close-edit`: with every node closed.
  fn close_edit(&mut self, out: &mut Vec<u8>) -> Result<(), EditorRefused> {
    let open = matches!(self.phase, Phase::Begun | Phase::Targeted | Phase::Rooted);
    if !open || !self.open.is_empty() || self.delta.is_some() {
      return Err(EditorRefused::OutOfOrder);
    }
    out.extend_from_slice(CLOSE_EDIT);
    self.phase = Phase::Closed;
    Ok(())
  }

  /// `abort-edit`: anywhere before the edit ended, a written `close-edit` included.
  fn abort_edit(&mut self, out: &mut Vec<u8>) -> Result<(), EditorRefused> {
    if self.phase == Phase::Ended {
      return Err(EditorRefused::OutOfOrder);
    }
    out.extend_from_slice(ABORT_EDIT);
    self.open.clear();
    self.delta = None;
    self.phase = Phase::Ended;
    Ok(())
  }

  /// The node an open or an add makes next, `make_token`'s token: its letter and the next number;
  /// refused past the most nodes open at once. Nothing changes until [`Self::push`].
  fn next_node(&self, kind: NodeKind) -> Result<OpenNode, EditorRefused> {
    if self.open.len() >= self.most_open || self.next_token.checked_add(1).is_none() {
      return Err(EditorRefused::TooDeep);
    }
    let letter = match kind {
      NodeKind::Directory => DIRECTORY_TOKEN,
      NodeKind::File => FILE_TOKEN,
    };
    Ok(OpenNode {
      kind,
      token: format!("{letter}{}", self.next_token),
    })
  }

  /// Opens the node [`Self::next_node`] made, its number counted.
  fn push(&mut self, node: OpenNode) {
    self.next_token = self.next_token.saturating_add(1);
    self.open.push(node);
  }

  /// The innermost open node, outside a delta and before the edit ended.
  fn innermost(&self) -> Result<&OpenNode, EditorRefused> {
    if self.phase != Phase::Rooted || self.delta.is_some() {
      return Err(EditorRefused::OutOfOrder);
    }
    self.open.last().ok_or(EditorRefused::OutOfOrder)
  }

  /// The innermost open node's token when it is a directory, outside a delta.
  fn directory(&self) -> Result<&str, EditorRefused> {
    let node = self.innermost()?;
    match node.kind {
      NodeKind::Directory => Ok(&node.token),
      NodeKind::File => Err(EditorRefused::OutOfOrder),
    }
  }

  /// The innermost open node's token when it is a file, outside a delta.
  fn file(&self) -> Result<&str, EditorRefused> {
    let node = self.innermost()?;
    match node.kind {
      NodeKind::File => Ok(&node.token),
      NodeKind::Directory => Err(EditorRefused::OutOfOrder),
    }
  }
}

/// Appends a string item.
fn string(bytes: &[u8], out: &mut Vec<u8>) -> Result<(), EditorRefused> {
  ra_svn::write(&Value::String(bytes), out).map_err(EditorRefused::Item)
}

/// Appends a number item.
fn number(value: u64, out: &mut Vec<u8>) -> Result<(), EditorRefused> {
  ra_svn::write(&Value::Number(value), out).map_err(EditorRefused::Item)
}

/// Appends a list holding the revision, or an empty one (`write_tuple_revision_opt` in a list).
fn optional_revision(revision: Option<Revision>, out: &mut Vec<u8>) -> Result<(), EditorRefused> {
  out.extend_from_slice(START_LIST);
  if let Some(revision) = revision {
    number(revision, out)?;
  }
  out.extend_from_slice(END_LIST);
  Ok(())
}

/// Appends one `textdelta-chunk` of the file named `token`.
fn chunk(token: &str, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), EditorRefused> {
  out.extend_from_slice(TEXTDELTA_CHUNK);
  string(token.as_bytes(), out)?;
  string(bytes, out)?;
  out.extend_from_slice(END_COMMAND);
  Ok(())
}

/// A digest in lowercase hexadecimal.
fn hex(digest: &[u8; 16]) -> [u8; 32] {
  let mut text = [0u8; 32];
  for (pair, &byte) in text.as_chunks_mut::<2>().0.iter_mut().zip(digest) {
    *pair = [
      hex_digit(byte >> NIBBLE_BITS),
      hex_digit(byte & NIBBLE_MASK),
    ];
  }
  text
}

/// One hexadecimal digit of a nibble.
fn hex_digit(nibble: u8) -> u8 {
  HEX_DIGITS.get(usize::from(nibble)).copied().unwrap_or(b'0')
}

/// An encoded window split as `window_handler` writes it: its integers, its instruction section and
/// its new-data section. Refused when the encoding does not hold its own lengths, which the encoder
/// never writes.
fn window_sections(encoded: &[u8]) -> Result<[&[u8]; 3], EditorRefused> {
  let malformed = EditorRefused::Window(SvndiffError::CorruptWindowHeader);
  let mut at = 0usize;
  let mut integers = [0u64; WINDOW_HEADER_INTEGERS];
  for slot in &mut integers {
    let rest = encoded.get(at..).ok_or(malformed)?;
    let (value, length) = read_integer(rest).ok_or(malformed)?;
    *slot = value;
    at = at.checked_add(length).ok_or(malformed)?;
  }
  let [_, _, _, instruction_length, new_data_length] = integers;
  let instruction_end = at
    .checked_add(usize::try_from(instruction_length).map_err(|_| malformed)?)
    .ok_or(malformed)?;
  let new_data_end = instruction_end
    .checked_add(usize::try_from(new_data_length).map_err(|_| malformed)?)
    .ok_or(malformed)?;
  if new_data_end != encoded.len() {
    return Err(malformed);
  }
  Ok([
    encoded.get(..at).ok_or(malformed)?,
    encoded.get(at..instruction_end).ok_or(malformed)?,
    encoded.get(instruction_end..).ok_or(malformed)?,
  ])
}

/// One svndiff integer at the start of `bytes`: its value and how many bytes it took; none when it
/// is cut short, longer than the format allows, or past 64 bits.
pub(super) fn read_integer(bytes: &[u8]) -> Option<(u64, usize)> {
  let mut value = 0u64;
  for (index, &byte) in bytes.iter().take(INTEGER_LIMIT).enumerate() {
    if value.leading_zeros() < GROUP_BITS {
      return None;
    }
    value = value.checked_shl(GROUP_BITS)? | u64::from(byte & !CONTINUATION_BIT);
    if byte & CONTINUATION_BIT == 0 {
      return Some((value, index.checked_add(1)?));
    }
  }
  None
}
