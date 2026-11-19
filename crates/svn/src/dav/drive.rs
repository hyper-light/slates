//! The update-report's answer (§4.17.4 piece 13, its second part; A-627): the editor drive in XML, written as
//! `reports/update.c` writes it in send-all mode, the mode a stock ra_serf asks for when the server allows bulk
//! updates.
//!
//! [`Drive`] takes the editor's calls in the order the server's delta makes them and writes each as `update.c`'s format
//! strings do. These are its lines at `d434dc14`:
//! - `maybe_start_update_report` (480-495): the header with `send-all` and `inline-props`.
//! - The target revision (510-512).
//! - `open_root` (541-556): `S:open-directory` and the checked-in href `send_vsn_url` writes (255-268).
//! - `add_helper` (360-410): `S:add-*` with its `bc-url`, `sha1-checksum` and copy source.
//! - `open_helper` (420-436).
//! - `upd_delete_entry` (565-575) and `absent_helper` (276-286).
//! - `send_propchange` (600-650): a value XML cannot carry in base64 with line breaks.
//! - `window_handler` (765-800): the delta in base64 with no line breaks, through `dav_svn__make_base64_output_stream`.
//! - `upd_close_file` (845-885): the text's MD5 in `S:prop`.
//! - `close_helper` (440-475).
//!
//! What to send, and in what order, is the server's: the delta between the client's reported state and the target,
//! computed over the index (piece 7's reporter). This module writes the calls it is given, byte for byte.
//!
//! No I/O, no clock, no randomness, no panic.

use super::DavRefusal;
use super::response::Body;
use super::xml::{base64_lines, is_xml_safe, quote};
use crate::index::{NodeKind, Revision};
use crate::svndiff::{Version, Window};

/// The XML declaration and `DEBUG_CR` the answer begins with.
/// Format: `mod_dav.h` `DAV_XML_HEADER`, then `DEBUG_CR`, a line feed (httpd 2.4.66).
const XML_HEADER: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n";

/// `update.c`'s `DIR_OR_FILE`: the word its element names carry for a node's kind.
fn dir_or_file(kind: NodeKind) -> &'static str {
  match kind {
    NodeKind::Directory => "directory",
    NodeKind::File => "file",
  }
}

/// An addition's attributes beyond its name (`add_helper`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Added<'a> {
  /// A directory's baseline-collection URL (`bc-url`).
  pub bc_url: Option<&'a str>,
  /// A file's SHA-1 (`sha1-checksum`).
  pub sha1: Option<&'a str>,
  /// The copy source: its path and revision.
  pub copy_from: Option<(&'a str, Revision)>,
}

/// An update-report's answer being written: the drive's calls, each appended within the body's room.
#[derive(Debug)]
pub struct Drive {
  /// The answer so far.
  body: Body,
}

impl Drive {
  /// Starts the answer (`maybe_start_update_report`): `send-all` and `inline-props` as the server decided.
  pub fn new(send_all: bool, inline_props: bool, room: usize) -> Result<Self, DavRefusal> {
    let mut body = Body::new(room);
    body.push(XML_HEADER)?;
    body.push(&format!(
      "<S:update-report xmlns:S=\"svn:\" xmlns:V=\"http://subversion.tigris.org/xmlns/dav/\" xmlns:D=\"DAV:\" {} {}>\n",
      if send_all { "send-all=\"true\"" } else { "" },
      if inline_props { "inline-props=\"true\"" } else { "" }
    ))?;
    Ok(Self { body })
  }

  /// The revision the drive brings the client to.
  pub fn target_revision(&mut self, revision: Revision) -> Result<(), DavRefusal> {
    self
      .body
      .push(&format!("<S:target-revision rev=\"{revision}\"/>\n"))
  }

  /// Opens the anchor (`open_root`), with its checked-in href when the report has no single target.
  pub fn open_root(&mut self, base: Revision, checked_in: Option<&str>) -> Result<(), DavRefusal> {
    self
      .body
      .push(&format!("<S:open-directory rev=\"{base}\">\n"))?;
    self.checked_in(checked_in)
  }

  /// Opens a child the client has (`open_helper`), with its checked-in href.
  pub fn open(
    &mut self,
    kind: NodeKind,
    name: &str,
    base: Revision,
    checked_in: &str,
  ) -> Result<(), DavRefusal> {
    self.body.push(&format!(
      "<S:open-{} name=\"{}\" rev=\"{base}\">\n",
      dir_or_file(kind),
      quote(name, true)
    ))?;
    self.checked_in(Some(checked_in))
  }

  /// Adds a child (`add_helper`), with its checked-in href.
  pub fn add(
    &mut self,
    kind: NodeKind,
    name: &str,
    added: Added<'_>,
    checked_in: &str,
  ) -> Result<(), DavRefusal> {
    let bc_url = added
      .bc_url
      .map(|url| format!(" bc-url=\"{}\"", quote(url, true)))
      .unwrap_or_default();
    let sha1 = added
      .sha1
      .map(|sha1| format!(" sha1-checksum=\"{sha1}\""))
      .unwrap_or_default();
    let copy = added
      .copy_from
      .map(|(path, revision)| {
        format!(
          " copyfrom-path=\"{}\" copyfrom-rev=\"{revision}\"",
          quote(path, true)
        )
      })
      .unwrap_or_default();
    self.body.push(&format!(
      "<S:add-{} name=\"{}\"{bc_url}{sha1}{copy}>\n",
      dir_or_file(kind),
      quote(name, true)
    ))?;
    self.checked_in(Some(checked_in))
  }

  /// Deletes a child (`upd_delete_entry`).
  pub fn delete_entry(&mut self, name: &str, revision: Revision) -> Result<(), DavRefusal> {
    self.body.push(&format!(
      "<S:delete-entry name=\"{}\" rev=\"{revision}\"/>\n",
      quote(name, true)
    ))
  }

  /// A child the client may not read (`absent_helper`).
  pub fn absent(&mut self, kind: NodeKind, name: &str) -> Result<(), DavRefusal> {
    self.body.push(&format!(
      "<S:absent-{} name=\"{}\"/>\n",
      dir_or_file(kind),
      quote(name, true)
    ))
  }

  /// Sets a property on the node open last (`send_propchange`): escaped as `svn_xml_escape_cdata_string` escapes, or in
  /// base64 with line breaks when XML cannot carry it.
  pub fn set_prop(&mut self, name: &str, value: &[u8]) -> Result<(), DavRefusal> {
    let name = quote(name, true);
    match std::str::from_utf8(value)
      .ok()
      .filter(|_| is_xml_safe(value))
    {
      Some(text) => self.body.push(&format!(
        "<S:set-prop name=\"{name}\">{}</S:set-prop>\n",
        quote(text, false).replace('\r', "&#13;")
      )),
      None => self.body.push(&format!(
        "<S:set-prop name=\"{name}\" encoding=\"base64\">\n{}</S:set-prop>\n",
        base64_lines(value)
      )),
    }
  }

  /// Removes a property from the node open last (`send_propchange`).
  pub fn remove_prop(&mut self, name: &str) -> Result<(), DavRefusal> {
    self.body.push(&format!(
      "<S:remove-prop name=\"{}\"/>\n",
      quote(name, true)
    ))
  }

  /// The open file's text as a delta (`window_handler`): its svndiff in `version`, in base64 with no line breaks.
  pub fn txdelta(
    &mut self,
    base_checksum: Option<&str>,
    windows: &[Window],
    version: Version,
  ) -> Result<(), DavRefusal> {
    let start = base_checksum
      .map(|checksum| format!("<S:txdelta base-checksum=\"{checksum}\">"))
      .unwrap_or_else(|| String::from("<S:txdelta>"));
    let encoded = super::listing::delta(windows, version)?;
    self.body.push(&format!("{start}{encoded}</S:txdelta>"))
  }

  /// Closes the open file (`upd_close_file`): its text's MD5, if it has one, then its element.
  pub fn close_file(&mut self, added: bool, md5: Option<&str>) -> Result<(), DavRefusal> {
    if let Some(md5) = md5 {
      self.body.push(&format!(
        "<S:prop><V:md5-checksum>{md5}</V:md5-checksum></S:prop>"
      ))?;
    }
    self.close(added, NodeKind::File)
  }

  /// Closes the open directory (`close_helper`).
  pub fn close_directory(&mut self, added: bool) -> Result<(), DavRefusal> {
    self.close(added, NodeKind::Directory)
  }

  /// Ends the answer: the bytes written.
  pub fn finish(mut self) -> Result<Vec<u8>, DavRefusal> {
    self.body.push("</S:update-report>\n")?;
    Ok(self.body.into_bytes())
  }

  /// `close_helper`'s element.
  fn close(&mut self, added: bool, kind: NodeKind) -> Result<(), DavRefusal> {
    let how = if added { "add" } else { "open" };
    self
      .body
      .push(&format!("</S:{how}-{}>\n", dir_or_file(kind)))
  }

  /// `send_vsn_url`: the node's checked-in href, if it has one to send.
  fn checked_in(&mut self, href: Option<&str>) -> Result<(), DavRefusal> {
    match href {
      Some(href) => self.body.push(&format!(
        "<D:checked-in><D:href>{}</D:href></D:checked-in>\n",
        quote(href, true)
      )),
      None => Ok(()),
    }
  }
}

#[cfg(test)]
mod tests;
