//! The editor drive in XML (§4.17.4 piece 13, its second part; A-627): [`Drive`] writes both answers that are an editor
//! drive, the update-report's as `reports/update.c` writes it and the replay report's as `reports/replay.c` writes it,
//! from one drive.
//!
//! The drive is the editor's calls (`svn_delta_editor_t`), one at a time, in the order the server's drive makes them:
//! A-616's drive over the index yields them as [`Call`]s, ra_svn's editor writes the same calls
//! (`report::editor`), [`Drive::call`] writes each here, and each method of [`Drive`] is one of them with Subversion's
//! own arguments. A path is the editor's, from the edit's root; a revision Subversion
//! passes as -1 is none, written `-1`; a property removal is no value; a text delta is its start, its windows one at a
//! time, and its end; a checksum is its digest's bytes, written in lower-case hex, an all-zero digest being none as
//! `svn_checksum_to_cstring` reads it (`checksum.c` 125-136). What each answer writes of a call:
//! - **The update-report** (`update.c` at `d434dc14`): the header (`maybe_start_update_report`, 478-497) with
//!   `send-all` and `inline-props`; the target revision (500-515); `open_root` (518-559), with the anchor's checked-in
//!   href unless the report names a target; `add_helper` (309-418) and `open_helper` (421-439), naming the entry's
//!   basename (`make_child_baton`, 200-227), with its checked-in href (`send_vsn_url`, 241-269), an added directory's
//!   `bc-url` and an added file's `sha1-checksum`; `upd_delete_entry` (562-575) and `absent_helper` (272-292), by
//!   basename; property changes (`send_propchange` and `upd_change_xxx_prop`, 604-716); the delta (`window_handler`,
//!   769-797, which writes `S:txdelta` at the first window, and `upd_apply_textdelta`, 799-839, which sends none
//!   outside send-all mode); `upd_close_file` (842-884) and `close_helper` (442-475); and the end (1546-1559). The
//!   hrefs are `dav_svn__build_uri`'s (`util.c` 235-292) over the node's repository path and the revision its
//!   [`Facts`] give, which must be `dav_svn__get_safe_cr`'s (`util.c` 196-232: the path's last change when the same
//!   node is there, else the target revision) for the href to name a resource that exists; the path encoded as
//!   `svn_path_uri_encode` encodes it (`path.c` 834-857,
//!   907-978); a bc-url is canonicalized and joined as `add_helper` does it (`svn_urlpath__canonicalize`,
//!   `svn_fspath__join`).
//! - **The replay report** (`replay.c` at `d434dc14`): the header, written at the first call (`maybe_start_report`,
//!   58-71), and the end (`end_report`, 73-80); each call an empty element naming the call's whole path (194-401); a
//!   property value always in base64 with line breaks (`change_file_or_dir_prop`, 147-187); the delta in
//!   `S:apply-textdelta` (308-337), whose end tag `replay.c` writes at the next call (`maybe_close_textdelta`), the
//!   same bytes as at the delta's end in every order a drive may make; and nothing for an absent entry, which
//!   `replay.c`'s editor leaves to the default editor.
//!
//! A delta is the svndiff `svn_txdelta_to_svndiff3` writes in the version the answer was opened with (DAV's
//! negotiation): its header with the first window, each version-2 section an LZ4 block when that is shorter
//! (`svndiff.c` 196-233, `compress_lz4.c` 58-69), in base64 with no line breaks (`dav_svn__make_base64_output_stream`),
//! each group written as its bytes arrive ([`Base64Stream`]).
//!
//! Stricter than `mod_dav_svn`, each stated:
//! - A call the drive's order does not allow where it comes (a second root, an entry's call outside a directory, a
//!   file's call on a directory, a close with its delta open, a window with no delta) is refused by kind before a byte
//!   of it is written ([`DavRefusal::DriveOrder`]), where the C editors write what they are given; an answer that ends
//!   with a node or a delta open is refused at its end.
//! - An aborted drive's bytes are never sent ([`DavRefusal::Aborted`]): the server answers the drive's refusal whole,
//!   as `mod_dav` answers an error met before it has sent a byte (`dav_method_report`), where `mod_dav_svn` flushes
//!   what its brigade passed on before the error.
//! - The repository's root path and special URI are canonical and URI-safe ([`DavRefusal::Unwritable`] otherwise), so
//!   a bc-url's canonicalization leaves them as they are; `dav_svn__build_uri` writes the root path unencoded.
//!
//! Not written (GAPS): the resource walk a pre-1.2 client asks for with `S:resource-walk` (`update.c` 1464-1544).
//!
//! No I/O, no clock, no randomness, no panic.

use std::borrow::Cow;

use super::response::Body;
use super::xml::{Base64Stream, base64_lines, is_xml_safe, quote};
use super::{DavRefusal, RevisionArg};
use crate::index::{NodeKind, Revision};
use crate::report::drive::{Call, Facts};
use crate::svndiff::{Compression, Encoder, Version, Window};

/// The XML declaration and `DEBUG_CR` an answer begins with.
/// Format: `mod_dav.h` `DAV_XML_HEADER`, then `DEBUG_CR`, a line feed (httpd 2.4.66).
const XML_HEADER: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n";

/// The bytes a URI path keeps as they are beside ASCII letters and digits; every other byte is `%` and two upper-case
/// hex digits.
/// Format: `path.c` `svn_uri__char_validity` (834-857): RFC 2396's `pchar`, `alphanum | mark | ":" | "@" | "&" | "=" |
/// "+" | "$" | ","`, with `/`.
const URI_SAFE_MARKS: &[u8] = b"!$&'()*+,-./:=@_~";

/// The hex digits a URI escape and a digest are written with, upper and lower case.
/// Format: `path.c` `uri_escape` writes `%%%02X`; `checksum.c` `svn__digest_to_cstring_display` writes
/// `0123456789abcdef`.
const HEX_DIGITS: (&[u8; 16], &[u8; 16]) = (b"0123456789ABCDEF", b"0123456789abcdef");

/// The bits of a hex digit.
/// Format: a hex digit is four bits.
const NIBBLE_BITS: u32 = 4;

/// The low four bits of a byte.
/// Format: `checksum.c` `digest[i] & 0x0f`.
const NIBBLE_MASK: u8 = 0x0f;

/// `update.c`'s `DIR_OR_FILE`: the word its element names carry for a node's kind.
fn dir_or_file(kind: NodeKind) -> &'static str {
  match kind {
    NodeKind::Directory => "directory",
    NodeKind::File => "file",
  }
}

/// The repository's URL space, from which `dav_svn__build_uri` builds the update-report's hrefs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uris {
  /// The repository's root path on the server (`repos->root_path`: `/repo`, or `/` for the server's root).
  pub root_path: String,
  /// The special URI's name (`SVNSpecialURI`, `!svn`).
  pub special_uri: String,
  /// Whether the report came to HTTPv2's me resource: its checked-in hrefs then name revision roots (`rvr`), else
  /// version resources (`ver`) (`send_vsn_url`).
  pub v2: bool,
}

/// What an update-report's answer is opened with beyond the drive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
  /// `send-all`: texts and properties inline (`uc->send_all`), which a stock client asks for when the server allows
  /// bulk updates.
  pub send_all: bool,
  /// `inline-props`: an addition's properties inline outside send-all mode (`uc->include_props`).
  pub inline_props: bool,
  /// Whether the report names a target (`S:update-target`), so the anchor's href is not sent (`upd_open_root`).
  pub targeted: bool,
  /// The URL space the hrefs are built in.
  pub uris: Uris,
}

/// Which answer a drive writes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Answer {
  /// The update-report's.
  Update(Update),
  /// The replay report's.
  Replay,
}

/// Where a drive stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
  /// Calls are taken.
  Driving,
  /// The edit was closed: only the answer's end is left.
  Closed,
  /// The drive was aborted: the answer is never sent.
  Aborted,
}

/// A node the drive has open, as far as its answer needs it (`update.c`'s `item_baton_t`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Opened {
  /// Its kind.
  kind: NodeKind,
  /// Whether it was added rather than opened.
  added: bool,
  /// Whether it was added with a copy source.
  copied: bool,
  /// Whether its text changed: a delta was applied to it.
  text_changed: bool,
  /// The base checksum its delta named.
  base_md5: Option<[u8; 16]>,
  /// Its text's SHA-1, from its facts.
  sha1: Option<[u8; 20]>,
  /// The property removals a copied addition keeps for its close outside send-all mode (`upd_change_xxx_prop`), each
  /// already written as its element.
  removed_props: Vec<String>,
}

impl Opened {
  /// A node just opened or added.
  fn new(kind: NodeKind, added: bool, copied: bool, sha1: Option<[u8; 20]>) -> Self {
    Self {
      kind,
      added,
      copied,
      text_changed: false,
      base_md5: None,
      sha1,
      removed_props: Vec::new(),
    }
  }
}

/// A text delta being written.
#[derive(Clone, Debug)]
struct Delta {
  /// The svndiff encoder, which keeps the source view a window may not slide back from.
  encoder: Encoder,
  /// Whether the svndiff header is in the stream: `svn_txdelta_to_svndiff3` writes it with the first window.
  header_sent: bool,
  /// Whether the delta's start tag is written: the update-report writes it at the first window.
  tag_sent: bool,
  /// The checksum of the base the delta applies to.
  base_md5: Option<[u8; 16]>,
  /// The base64 written so far, less a group not yet whole.
  base64: Base64Stream,
  /// Whether its windows are dropped: an update-report outside send-all mode sends no delta.
  dropped: bool,
}

/// An editor drive's answer being written: the drive's calls, each appended within the answer's room.
#[derive(Debug)]
pub struct Drive {
  /// The answer so far.
  body: Body,
  /// Which answer it is.
  answer: Answer,
  /// The svndiff version its deltas are written in.
  version: Version,
  /// Whether its header is written: the update-report's at once, the replay report's at the first call.
  started: bool,
  /// Whether the edit's root was opened.
  rooted: bool,
  /// The nodes open, the innermost last.
  opened: Vec<Opened>,
  /// The delta being written, if one is.
  delta: Option<Delta>,
  /// The bytes the property removals kept for a close will take, held against the room.
  reserved: usize,
  /// Where the drive stands.
  stage: Stage,
}

impl Drive {
  /// Opens an update-report's answer: its header written at once, with `send-all` and `inline-props` as the server
  /// decided (`maybe_start_update_report`, called by every first call). Refused when the root path or the special URI
  /// is not canonical and URI-safe.
  pub fn update(update: Update, version: Version, room: usize) -> Result<Self, DavRefusal> {
    uri_space(&update.uris)?;
    let mut body = Body::new(room);
    body.push(XML_HEADER)?;
    body.push(&format!(
      "<S:update-report xmlns:S=\"svn:\" xmlns:V=\"http://subversion.tigris.org/xmlns/dav/\" xmlns:D=\"DAV:\" {} {}>\n",
      if update.send_all { "send-all=\"true\"" } else { "" },
      if update.inline_props { "inline-props=\"true\"" } else { "" }
    ))?;
    Ok(Self::with(body, Answer::Update(update), version, true))
  }

  /// Opens a replay report's answer: nothing is written before the drive's first call (`maybe_start_report`).
  pub fn replay(version: Version, room: usize) -> Self {
    Self::with(Body::new(room), Answer::Replay, version, false)
  }

  /// Writes one call of the drive (A-616's [`Call`]), as the method of its name writes it: the one drive that feeds
  /// ra_svn's editor feeds both answers here.
  pub fn call(&mut self, call: Call<'_>) -> Result<(), DavRefusal> {
    match call {
      Call::TargetRevision(revision) => self.target_revision(revision),
      Call::OpenRoot { base, node } => self.open_root(base, &node),
      Call::Open {
        kind,
        path,
        base,
        node,
      } => self.open(kind, path, base, &node),
      Call::Add {
        kind,
        path,
        copy_from,
        node,
      } => self.add(kind, path, copy_from, &node),
      Call::Delete { path, revision } => self.delete_entry(path, revision),
      Call::Absent { kind, path } => self.absent(kind, path),
      Call::ChangeProperty { name, value } => self.change_property(name, value),
      Call::ApplyTextDelta { base_md5 } => self.apply_textdelta(base_md5),
      Call::Window(window) => self.window(window),
      Call::TextDeltaEnd => self.textdelta_end(),
      Call::CloseFile { md5 } => self.close_file(md5),
      Call::CloseDirectory => self.close_directory(),
      Call::CloseEdit => self.close_edit(),
      Call::AbortEdit => {
        self.abort_edit();
        Ok(())
      }
    }
  }

  /// The revision the drive brings the client to (`set_target_revision`), before the root is opened.
  pub fn target_revision(&mut self, revision: Revision) -> Result<(), DavRefusal> {
    order(!self.rooted && self.driving(), "target-revision")?;
    self.start()?;
    self
      .body
      .push(&format!("<S:target-revision rev=\"{revision}\"/>\n"))
  }

  /// Opens the edit's root (`open_root`), once: the anchor's facts give its checked-in href.
  pub fn open_root(&mut self, base: RevisionArg, node: &Facts<'_>) -> Result<(), DavRefusal> {
    order(!self.rooted && self.driving(), "open-root")?;
    self.start()?;
    let base = revision_text(base);
    let text = match &self.answer {
      Answer::Update(update) if update.targeted => format!("<S:open-directory rev=\"{base}\">\n"),
      Answer::Update(update) => format!(
        "<S:open-directory rev=\"{base}\">\n{}",
        checked_in(&update.uris, node)
      ),
      Answer::Replay => format!("<S:open-root rev=\"{base}\"/>\n"),
    };
    self.body.push(&text)?;
    self.rooted = true;
    self
      .opened
      .push(Opened::new(NodeKind::Directory, false, false, node.sha1));
    Ok(())
  }

  /// Opens an entry the client has, in the directory open last (`open_directory`, `open_file`).
  pub fn open(
    &mut self,
    kind: NodeKind,
    path: &str,
    base: RevisionArg,
    node: &Facts<'_>,
  ) -> Result<(), DavRefusal> {
    self.in_directory("open")?;
    let base = revision_text(base);
    let text = match &self.answer {
      Answer::Update(update) => format!(
        "<S:open-{} name=\"{}\" rev=\"{base}\">\n{}",
        dir_or_file(kind),
        quote(basename(path), true),
        checked_in(&update.uris, node)
      ),
      Answer::Replay => format!(
        "<S:open-{} name=\"{}\" rev=\"{base}\"/>\n",
        dir_or_file(kind),
        quote(path, true)
      ),
    };
    self.body.push(&text)?;
    self.opened.push(Opened::new(kind, false, false, node.sha1));
    Ok(())
  }

  /// Adds an entry to the directory open last (`add_directory`, `add_file`), with its copy source when it has one.
  pub fn add(
    &mut self,
    kind: NodeKind,
    path: &str,
    copy_from: Option<(&str, Revision)>,
    node: &Facts<'_>,
  ) -> Result<(), DavRefusal> {
    self.in_directory("add")?;
    let text = match &self.answer {
      Answer::Update(update) => update_add(update, kind, path, copy_from, node),
      Answer::Replay => replay_add(kind, path, copy_from),
    };
    self.body.push(&text)?;
    self
      .opened
      .push(Opened::new(kind, true, copy_from.is_some(), node.sha1));
    Ok(())
  }

  /// Deletes an entry of the directory open last (`delete_entry`).
  pub fn delete_entry(&mut self, path: &str, revision: RevisionArg) -> Result<(), DavRefusal> {
    self.in_directory("delete-entry")?;
    let name = match self.answer {
      Answer::Update(_) => basename(path),
      Answer::Replay => path,
    };
    self.body.push(&format!(
      "<S:delete-entry name=\"{}\" rev=\"{}\"/>\n",
      quote(name, true),
      revision_text(revision)
    ))
  }

  /// An entry of the directory open last that the client may not read (`absent_directory`, `absent_file`); the replay
  /// answer writes nothing.
  pub fn absent(&mut self, kind: NodeKind, path: &str) -> Result<(), DavRefusal> {
    self.in_directory("absent")?;
    match self.answer {
      Answer::Update(_) => self.body.push(&format!(
        "<S:absent-{} name=\"{}\"/>\n",
        dir_or_file(kind),
        quote(basename(path), true)
      )),
      Answer::Replay => Ok(()),
    }
  }

  /// Sets a property of the node open last, or removes it when there is no value (`change_dir_prop`,
  /// `change_file_prop`).
  pub fn change_property(&mut self, name: &str, value: Option<&[u8]>) -> Result<(), DavRefusal> {
    let innermost = self.opened.last().map(|node| node.kind);
    let Some(kind) = innermost.filter(|_| self.driving() && self.delta.is_none()) else {
      return Err(DavRefusal::DriveOrder {
        call: "change-property",
      });
    };
    let modes = match &self.answer {
      Answer::Update(update) => Some((update.send_all, update.inline_props)),
      Answer::Replay => None,
    };
    match modes {
      Some((send_all, inline_props)) => self.update_property(send_all, inline_props, name, value),
      None => self.body.push(&replay_property(kind, name, value)),
    }
  }

  /// Starts a delta against the text of the file open last (`apply_textdelta`), once a file.
  pub fn apply_textdelta(&mut self, base_md5: Option<[u8; 16]>) -> Result<(), DavRefusal> {
    let on_file = self.delta.is_none()
      && self
        .opened
        .last()
        .is_some_and(|node| node.kind == NodeKind::File && !node.text_changed);
    order(self.driving() && on_file, "apply-textdelta")?;
    let dropped = matches!(&self.answer, Answer::Update(update) if !update.send_all);
    if let Answer::Replay = self.answer {
      self
        .body
        .push(&match hex_digest(&base_md5.unwrap_or_default()) {
          Some(hex) => format!("<S:apply-textdelta checksum=\"{hex}\">"),
          None => String::from("<S:apply-textdelta>"),
        })?;
    }
    if let Some(node) = self.opened.last_mut() {
      node.text_changed = true;
      node.base_md5 = base_md5;
    }
    self.delta = Some(Delta {
      encoder: Encoder::new(self.version),
      header_sent: false,
      tag_sent: matches!(self.answer, Answer::Replay),
      base_md5,
      base64: Base64Stream::new(),
      dropped,
    });
    Ok(())
  }

  /// One window of the delta started last, in order (the delta's window handler).
  pub fn window(&mut self, window: &Window) -> Result<(), DavRefusal> {
    let driving = self.driving();
    let Some(delta) = self.delta.as_mut().filter(|_| driving) else {
      return Err(DavRefusal::DriveOrder { call: "window" });
    };
    if delta.dropped {
      return Ok(());
    }
    let mut text = delta_start(delta);
    let mut bytes = Vec::new();
    if !delta.header_sent {
      bytes.extend_from_slice(&delta.encoder.header());
      delta.header_sent = true;
    }
    delta
      .encoder
      .window(window, Compression::Lz4, &mut bytes)
      .map_err(|error| DavRefusal::Unwritable {
        what: "svndiff window",
        value: format!("{error:?}"),
      })?;
    text.push_str(&delta.base64.push(&bytes));
    self.body.push(&text)
  }

  /// Ends the delta started last (the window handler's last call, with no window).
  pub fn textdelta_end(&mut self) -> Result<(), DavRefusal> {
    let driving = self.driving();
    let Some(mut delta) = self.delta.take().filter(|_| driving) else {
      return Err(DavRefusal::DriveOrder {
        call: "textdelta-end",
      });
    };
    if delta.dropped {
      return Ok(());
    }
    let mut text = delta_start(&mut delta);
    if !delta.header_sent {
      text.push_str(&delta.base64.push(&delta.encoder.header()));
    }
    text.push_str(&delta.base64.finish());
    text.push_str(match self.answer {
      Answer::Update(_) => "</S:txdelta>",
      Answer::Replay => "</S:apply-textdelta>\n",
    });
    self.body.push(&text)
  }

  /// Closes the file open last (`close_file`), its text's MD5 given.
  pub fn close_file(&mut self, md5: [u8; 16]) -> Result<(), DavRefusal> {
    let node = self.close_node(NodeKind::File, "close-file")?;
    let text = match &self.answer {
      Answer::Update(update) => update_close_file(update.send_all, &node, md5),
      Answer::Replay => match hex_digest(&md5) {
        Some(hex) => format!("<S:close-file checksum=\"{hex}\"/>\n"),
        None => String::from("<S:close-file/>\n"),
      },
    };
    self.body.push(&text)
  }

  /// Closes the directory open last (`close_directory`).
  pub fn close_directory(&mut self) -> Result<(), DavRefusal> {
    let node = self.close_node(NodeKind::Directory, "close-directory")?;
    let text = match self.answer {
      Answer::Update(_) => close_helper(&node),
      Answer::Replay => String::from("<S:close-directory/>\n"),
    };
    self.body.push(&text)
  }

  /// Closes the edit (`close_edit`), every node closed; neither answer writes anything for it.
  pub fn close_edit(&mut self) -> Result<(), DavRefusal> {
    order(
      self.driving() && self.opened.is_empty() && self.delta.is_none(),
      "close-edit",
    )?;
    self.stage = Stage::Closed;
    Ok(())
  }

  /// Aborts the drive (`abort_edit`): a refusal was met mid-drive, so the answer is never sent and its end refuses.
  pub fn abort_edit(&mut self) {
    self.stage = Stage::Aborted;
  }

  /// Takes what is written so far into `out`, as a streamed answer sends it a slice at a time (A-681): what follows is
  /// written within the room afresh. Nothing of an aborted drive is taken, since its bytes are never sent.
  pub fn take_written(&mut self, out: &mut Vec<u8>) {
    if self.stage != Stage::Aborted {
      out.extend_from_slice(&self.body.drain());
    }
  }

  /// Ends the answer: the bytes to send. Refused for an aborted drive, and while a node or a delta is open.
  pub fn finish(mut self) -> Result<Vec<u8>, DavRefusal> {
    if self.stage == Stage::Aborted {
      return Err(DavRefusal::Aborted);
    }
    order(self.opened.is_empty() && self.delta.is_none(), "finish")?;
    self.body.push(match self.answer {
      Answer::Update(_) => "</S:update-report>\n",
      Answer::Replay => "</S:editor-report>\n",
    })?;
    Ok(self.body.into_bytes())
  }

  /// A drive with `body` begun.
  fn with(body: Body, answer: Answer, version: Version, started: bool) -> Self {
    Self {
      body,
      answer,
      version,
      started,
      rooted: false,
      opened: Vec::new(),
      delta: None,
      reserved: 0,
      stage: Stage::Driving,
    }
  }

  /// Whether calls are taken.
  fn driving(&self) -> bool {
    self.stage == Stage::Driving
  }

  /// `call` on an entry of the directory open last, refused unless one is open and no delta is.
  fn in_directory(&self, call: &'static str) -> Result<(), DavRefusal> {
    let directory = self
      .opened
      .last()
      .is_some_and(|node| node.kind == NodeKind::Directory);
    order(self.driving() && self.delta.is_none() && directory, call)
  }

  /// Writes the replay report's header if it is not yet written (`maybe_start_report`).
  fn start(&mut self) -> Result<(), DavRefusal> {
    if self.started {
      return Ok(());
    }
    self.body.push(XML_HEADER)?;
    self.body.push("<S:editor-report xmlns:S=\"svn:\">\n")?;
    self.started = true;
    Ok(())
  }

  /// Closes the node open last, refused unless it is of `kind` and its delta is ended.
  fn close_node(&mut self, kind: NodeKind, call: &'static str) -> Result<Opened, DavRefusal> {
    let last = self.opened.last().is_some_and(|node| node.kind == kind);
    order(self.driving() && self.delta.is_none() && last, call)?;
    let node = self.opened.pop().ok_or(DavRefusal::DriveOrder { call })?;
    let kept: usize = node.removed_props.iter().map(String::len).sum();
    self.reserved = self.reserved.saturating_sub(kept);
    Ok(node)
  }

  /// An update-report's property change (`upd_change_xxx_prop`): sent in send-all mode; outside it, an addition's
  /// value only when properties are inline, a copied addition's removal kept for its close, and any other change sent.
  fn update_property(
    &mut self,
    send_all: bool,
    inline_props: bool,
    name: &str,
    value: Option<&[u8]>,
  ) -> Result<(), DavRefusal> {
    let Some(node) = self.opened.last() else {
      return Err(DavRefusal::DriveOrder {
        call: "change-property",
      });
    };
    if send_all || !node.added {
      return self.body.push(&send_propchange(name, value));
    }
    if value.is_some() && inline_props {
      return self.body.push(&send_propchange(name, value));
    }
    if node.copied && value.is_none() {
      return self.keep_removal(name);
    }
    Ok(())
  }

  /// Keeps a copied addition's property removal for its close, its bytes held against the room now.
  fn keep_removal(&mut self, name: &str) -> Result<(), DavRefusal> {
    let element = send_propchange(name, None);
    let needed = self.reserved.saturating_add(element.len());
    if needed > self.body.left() {
      return Err(DavRefusal::OutputRoom {
        needed,
        room: self.body.left(),
      });
    }
    self.reserved = needed;
    if let Some(node) = self.opened.last_mut() {
      node.removed_props.push(element);
    }
    Ok(())
  }
}

/// `call`, refused unless the drive's order `allowed` it where it came.
fn order(allowed: bool, call: &'static str) -> Result<(), DavRefusal> {
  if allowed {
    Ok(())
  } else {
    Err(DavRefusal::DriveOrder { call })
  }
}

/// A revision as `%ld` writes Subversion's: none is `-1`.
fn revision_text(revision: RevisionArg) -> String {
  revision.map_or_else(|| String::from("-1"), |revision| revision.to_string())
}

/// The last name of an editor's path (`svn_relpath_basename`).
fn basename(path: &str) -> &str {
  path.rsplit('/').next().unwrap_or(path)
}

/// `digest` in lower-case hex, or none for an all-zero digest (`svn_checksum_to_cstring`).
fn hex_digest<const N: usize>(digest: &[u8; N]) -> Option<String> {
  if digest.iter().all(|byte| *byte == 0) {
    return None;
  }
  let (_, lower) = HEX_DIGITS;
  let mut text = String::with_capacity(N.saturating_mul(2));
  for byte in digest {
    for nibble in [
      byte.checked_shr(NIBBLE_BITS).unwrap_or(0),
      byte & NIBBLE_MASK,
    ] {
      text.push(
        lower
          .get(usize::from(nibble))
          .map_or('0', |digit| char::from(*digit)),
      );
    }
  }
  Some(text)
}

/// `path` encoded as `svn_path_uri_encode` encodes it: each byte but letters, digits and the marks a path segment keeps
/// as `%` and two upper-case hex digits.
pub(crate) fn uri_encode(path: &str) -> Cow<'_, str> {
  let safe = |byte: &u8| byte.is_ascii_alphanumeric() || URI_SAFE_MARKS.contains(byte);
  if path.as_bytes().iter().all(safe) {
    return Cow::Borrowed(path);
  }
  let (upper, _) = HEX_DIGITS;
  // Format: an escaped byte is three, `%` and its two hex digits.
  let mut text = String::with_capacity(path.len().saturating_mul(3));
  for byte in path.bytes() {
    if safe(&byte) {
      text.push(char::from(byte));
      continue;
    }
    text.push('%');
    for nibble in [
      byte.checked_shr(NIBBLE_BITS).unwrap_or(0),
      byte & NIBBLE_MASK,
    ] {
      text.push(
        upper
          .get(usize::from(nibble))
          .map_or('0', |digit| char::from(*digit)),
      );
    }
  }
  Cow::Owned(text)
}

/// The root path and special URI, refused unless canonical and URI-safe: the root `/` or `/` and names with no empty,
/// `.` or `..` name and no trailing `/`; the special URI one such name.
fn uri_space(uris: &Uris) -> Result<(), DavRefusal> {
  let safe = |text: &str| matches!(uri_encode(text), Cow::Borrowed(_));
  let name_ok = |name: &str| !name.is_empty() && name != "." && name != "..";
  let root_ok = uris.root_path == "/"
    || uris
      .root_path
      .strip_prefix('/')
      .is_some_and(|rest| rest.split('/').all(name_ok));
  if !root_ok || !safe(&uris.root_path) {
    return Err(DavRefusal::Unwritable {
      what: "repository root path",
      value: uris.root_path.clone(),
    });
  }
  let special_ok = name_ok(&uris.special_uri) && !uris.special_uri.contains('/');
  if !special_ok || !safe(&uris.special_uri) {
    return Err(DavRefusal::Unwritable {
      what: "special URI",
      value: uris.special_uri.clone(),
    });
  }
  Ok(())
}

/// The root path as `dav_svn__build_uri` prefixes a URI with it: nothing for the server's root.
fn root_prefix(uris: &Uris) -> &str {
  if uris.root_path == "/" {
    ""
  } else {
    &uris.root_path
  }
}

/// `send_vsn_url`: the node's checked-in href, a version resource or, by HTTPv2, a revision root's path.
fn checked_in(uris: &Uris, node: &Facts<'_>) -> String {
  let resource = if uris.v2 { "rvr" } else { "ver" };
  let href = format!(
    "{}/{}/{resource}/{}{}",
    root_prefix(uris),
    uris.special_uri,
    node.created_revision,
    uri_encode(node.repository_path)
  );
  format!(
    "<D:checked-in><D:href>{}</D:href></D:checked-in>\n",
    quote(&href, true)
  )
}

/// An added directory's baseline-collection URL (`add_helper` 339-366): the collection's root, canonical, joined with
/// the node's repository path less its leading `/`, encoded.
fn bc_url(uris: &Uris, node: &Facts<'_>) -> String {
  let collection = format!(
    "{}/{}/bc/{}",
    root_prefix(uris),
    uris.special_uri,
    node.created_revision
  );
  match node
    .repository_path
    .strip_prefix('/')
    .filter(|rest| !rest.is_empty())
  {
    Some(rest) => format!("{collection}/{}", uri_encode(rest)),
    None => collection,
  }
}

/// The update-report's addition (`add_helper`): the basename, a directory's bc-url or a file's SHA-1, the copy source,
/// then the checked-in href.
fn update_add(
  update: &Update,
  kind: NodeKind,
  path: &str,
  copy_from: Option<(&str, Revision)>,
  node: &Facts<'_>,
) -> String {
  let checksum_or_collection = match kind {
    NodeKind::Directory => format!(" bc-url=\"{}\"", quote(&bc_url(&update.uris, node), true)),
    NodeKind::File => node
      .sha1
      .as_ref()
      .and_then(hex_digest)
      .map(|hex| format!(" sha1-checksum=\"{hex}\""))
      .unwrap_or_default(),
  };
  let copy = copy_from
    .map(|(from, revision)| {
      format!(
        " copyfrom-path=\"{}\" copyfrom-rev=\"{revision}\"",
        quote(from, true)
      )
    })
    .unwrap_or_default();
  format!(
    "<S:add-{} name=\"{}\"{checksum_or_collection}{copy}>\n{}",
    dir_or_file(kind),
    quote(basename(path), true),
    checked_in(&update.uris, node)
  )
}

/// The replay report's addition (`add_file_or_directory`): the whole path, with the copy source when there is one.
fn replay_add(kind: NodeKind, path: &str, copy_from: Option<(&str, Revision)>) -> String {
  match copy_from {
    Some((from, revision)) => format!(
      "<S:add-{} name=\"{}\" copyfrom-path=\"{}\" copyfrom-rev=\"{revision}\"/>\n",
      dir_or_file(kind),
      quote(path, true),
      quote(from, true)
    ),
    None => format!(
      "<S:add-{} name=\"{}\"/>\n",
      dir_or_file(kind),
      quote(path, true)
    ),
  }
}

/// The update-report's property change (`send_propchange`): a value XML can carry escaped as
/// `svn_xml_escape_cdata_string` escapes it, any other in base64 with line breaks, a removal by name.
fn send_propchange(name: &str, value: Option<&[u8]>) -> String {
  let name = quote(name, true);
  let Some(value) = value else {
    return format!("<S:remove-prop name=\"{name}\"/>\n");
  };
  match std::str::from_utf8(value)
    .ok()
    .filter(|_| is_xml_safe(value))
  {
    Some(text) => format!(
      "<S:set-prop name=\"{name}\">{}</S:set-prop>\n",
      quote(text, false).replace('\r', "&#13;")
    ),
    None => format!(
      "<S:set-prop name=\"{name}\" encoding=\"base64\">\n{}</S:set-prop>\n",
      base64_lines(value)
    ),
  }
}

/// The replay report's property change (`change_file_or_dir_prop`): `dir` or `file` by the node's kind, the value in
/// base64 with line breaks, a removal marked `del`.
fn replay_property(kind: NodeKind, name: &str, value: Option<&[u8]>) -> String {
  let what = match kind {
    NodeKind::Directory => "dir",
    NodeKind::File => "file",
  };
  let name = quote(name, true);
  match value {
    Some(value) => format!(
      "<S:change-{what}-prop name=\"{name}\">{}</S:change-{what}-prop>\n",
      base64_lines(value)
    ),
    None => format!("<S:change-{what}-prop name=\"{name}\" del=\"true\"/>\n"),
  }
}

/// The update-report's delta start tag, when it is not yet written (`window_handler`'s first window): its base
/// checksum named when it has one.
fn delta_start(delta: &mut Delta) -> String {
  if delta.tag_sent {
    return String::new();
  }
  delta.tag_sent = true;
  match hex_digest(&delta.base_md5.unwrap_or_default()) {
    Some(hex) => format!("<S:txdelta base-checksum=\"{hex}\">"),
    None => String::from("<S:txdelta>"),
  }
}

/// The update-report's file close (`upd_close_file`): outside send-all mode, a changed file it opened is fetched; the
/// text's MD5; then the close (`close_helper`).
fn update_close_file(send_all: bool, node: &Opened, md5: [u8; 16]) -> String {
  let mut text = String::new();
  if !send_all && !node.added && node.text_changed {
    let base = hex_digest(&node.base_md5.unwrap_or_default())
      .map(|hex| format!(" base-checksum=\"{hex}\""))
      .unwrap_or_default();
    let sha1 = node
      .sha1
      .as_ref()
      .and_then(hex_digest)
      .map(|hex| format!(" sha1-checksum=\"{hex}\""))
      .unwrap_or_default();
    text.push_str(&format!("<S:fetch-file{base}{sha1}/>\n"));
  }
  if let Some(hex) = hex_digest(&md5) {
    text.push_str(&format!(
      "<S:prop><V:md5-checksum>{hex}</V:md5-checksum></S:prop>"
    ));
  }
  text.push_str(&close_helper(node));
  text
}

/// `close_helper`: a copied addition's kept removals, then its element's end.
fn close_helper(node: &Opened) -> String {
  let mut text = String::new();
  if node.copied {
    for removal in &node.removed_props {
      text.push_str(removal);
    }
  }
  let how = if node.added { "add" } else { "open" };
  text.push_str(&format!("</S:{how}-{}>\n", dir_or_file(node.kind)));
  text
}

#[cfg(test)]
mod tests;
