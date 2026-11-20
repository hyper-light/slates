//! `list-report` and `file-revs-report` (§4.17.4 piece 13, its second part; A-627): read as `reports/list.c` and
//! `reports/file-revs.c` read them, and answered byte for byte as they write the answers.
//!
//! A `list-report` names a path, a revision, a depth, the name patterns, and which entry fields to send through its
//! `S:prop` names (`DAV:resourcetype` the kind, `DAV:getcontentlength` the size, `deadprop-count` whether there are
//! properties, `DAV:version-name` the created revision, `DAV:creationdate` the date, `DAV:creator-displayname` the
//! author, `DAV:allprop` all). A `file-revs-report` names a path and a revision range; each revision of the file is
//! answered with its revision properties, its property changes and its text as an svndiff delta in base64 with no
//! line breaks (`dav_svn__make_base64_output_stream` passes `svn_base64_encode2` `FALSE`), where a property value XML
//! cannot carry is base64 with line breaks, as `send_prop` writes it.
//!
//! No I/O, no clock, no randomness, no panic.

use super::request::SvnDepth;
use super::response::{Body, svn_time};
use super::xml::{self, Element, base64_lines, is_xml_safe, quote};
use super::{Bounds, DavRefusal, RevisionArg, SVN};
use crate::index::{Revision, is_canonical};
use crate::lock::AprTime;
use crate::svndiff::{Compression, Encoder, Version, Window};

/// The XML declaration and `DEBUG_CR` every answer begins with.
/// Format: `mod_dav.h` `DAV_XML_HEADER`, then `DEBUG_CR`, a line feed (httpd 2.4.66).
const XML_HEADER: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n";

/// Which entry fields a `list-report` sends (`SVN_DIRENT_*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirentFields {
  /// The node kind (`DAV:resourcetype`).
  pub kind: bool,
  /// The size (`DAV:getcontentlength`).
  pub size: bool,
  /// Whether it has properties (`deadprop-count` in `mod_dav_svn`'s namespace).
  pub has_props: bool,
  /// The created revision (`DAV:version-name`).
  pub created_rev: bool,
  /// The date (`DAV:creationdate`).
  pub time: bool,
  /// The last author (`DAV:creator-displayname`).
  pub last_author: bool,
}

/// A `list-report`'s request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListReport {
  /// `S:path`, relative to the report's target.
  pub path: String,
  /// `S:revision`.
  pub revision: RevisionArg,
  /// `S:depth`; `unknown` when absent.
  pub depth: SvnDepth,
  /// The `S:pattern`s; `S:no-patterns` is an empty list, and none given is `None`.
  pub patterns: Option<Vec<String>>,
  /// The fields asked for.
  pub fields: DirentFields,
}

/// A `file-revs-report`'s request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRevsReport {
  /// `S:path`, relative to the report's target.
  pub path: String,
  /// `S:start-revision`.
  pub start: RevisionArg,
  /// `S:end-revision`.
  pub end: RevisionArg,
  /// `S:include-merged-revisions` present.
  pub include_merged_revisions: bool,
}

/// Reads a `list-report`; a path is required ("Request was missing the path argument").
pub fn list_report(body: &[u8], bounds: &Bounds) -> Result<ListReport, DavRefusal> {
  let root = svn_root(body, bounds, "list-report")?;
  let mut path = None;
  let mut report = ListReport {
    path: String::new(),
    revision: None,
    depth: SvnDepth::Unknown,
    patterns: None,
    fields: DirentFields::default(),
  };
  for child in root.elements().filter(|child| child.namespace == SVN) {
    match child.name.as_str() {
      "path" => path = Some(canonical(child.text())?),
      "revision" => report.revision = revision(&child.trimmed_text())?,
      "depth" => report.depth = SvnDepth::from_word(&child.trimmed_text()),
      "no-patterns" => report.patterns = Some(Vec::new()),
      "pattern" => report
        .patterns
        .get_or_insert_with(Vec::new)
        .push(child.text()),
      "prop" => ask(&mut report.fields, &child.text()),
      _ => {}
    }
  }
  report.path = path.ok_or(DavRefusal::Missing { element: "path" })?;
  Ok(report)
}

/// Marks the field an `S:prop` name asks for, as `list.c` maps it.
fn ask(fields: &mut DirentFields, name: &str) {
  match name {
    "DAV:resourcetype" => fields.kind = true,
    "DAV:getcontentlength" => fields.size = true,
    "http://subversion.tigris.org/xmlns/dav/deadprop-count" => fields.has_props = true,
    "DAV:version-name" => fields.created_rev = true,
    "DAV:creationdate" => fields.time = true,
    "DAV:creator-displayname" => fields.last_author = true,
    "DAV:allprop" => {
      *fields = DirentFields {
        kind: true,
        size: true,
        has_props: true,
        created_rev: true,
        time: true,
        last_author: true,
      }
    }
    _ => {}
  }
}

/// Reads a `file-revs-report`; a path is required ("Not all parameters passed").
pub fn file_revs_report(body: &[u8], bounds: &Bounds) -> Result<FileRevsReport, DavRefusal> {
  file_revs_from(&svn_root(body, bounds, "file-revs-report")?)
}

/// Reads a `file-revs-report` whose root is read already (`request::report`'s dispatch, A-726).
pub(crate) fn file_revs_from(root: &Element) -> Result<FileRevsReport, DavRefusal> {
  let mut path = None;
  let (mut start, mut end, mut include_merged_revisions) = (None, None, false);
  for child in root.elements().filter(|child| child.namespace == SVN) {
    match child.name.as_str() {
      "start-revision" => start = revision(&child.trimmed_text())?,
      "end-revision" => end = revision(&child.trimmed_text())?,
      "include-merged-revisions" => include_merged_revisions = true,
      "path" => path = Some(canonical(child.text())?),
      _ => {}
    }
  }
  Ok(FileRevsReport {
    path: path.ok_or(DavRefusal::Missing { element: "path" })?,
    start,
    end,
    include_merged_revisions,
  })
}

/// The root of a report named `name` in `svn:`.
fn svn_root(body: &[u8], bounds: &Bounds, name: &'static str) -> Result<Element, DavRefusal> {
  let root = xml::parse(body, bounds)?;
  if root.is(SVN, name) {
    Ok(root)
  } else {
    Err(DavRefusal::UnknownReport {
      namespace: root.namespace,
      name: root.name,
    })
  }
}

/// A revision as a body writes it: decimal, negative for none.
fn revision(text: &str) -> Result<RevisionArg, DavRefusal> {
  let digits = text.strip_prefix('-').unwrap_or(text);
  let malformed = || DavRefusal::Malformed {
    element: "revision",
    text: text.to_owned(),
  };
  if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
    return Err(malformed());
  }
  let value: Revision = digits.parse().map_err(|_| malformed())?;
  Ok(if text.starts_with('-') {
    None
  } else {
    Some(value)
  })
}

/// A path that passes `dav_svn__test_canonical` as this server reads it (the request module's rule).
fn canonical(path: String) -> Result<String, DavRefusal> {
  let relative_ok = path
    .split('/')
    .all(|name| !name.is_empty() && name != "." && name != ".." && !name.contains(['\n', '\0']));
  if path.is_empty() || (path.starts_with('/') && is_canonical(&path)) || relative_ok {
    Ok(path)
  } else {
    Err(DavRefusal::NotCanonical { path })
  }
}

/// One entry of a `list-report`'s answer; each field is sent when the request asked for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListItem {
  /// The path, relative to the report's target.
  pub path: String,
  /// `svn_node_kind_to_word`'s word; `unknown` when the kind was not asked for.
  pub kind: &'static str,
  /// The size.
  pub size: Option<u64>,
  /// Whether it has properties.
  pub has_props: Option<bool>,
  /// The created revision.
  pub created_rev: Option<Revision>,
  /// The date.
  pub date: Option<AprTime>,
  /// The last author, already escaped for a client that is not svn (`dav_svn__fuzzy_escape_author`).
  pub author: Option<String>,
}

/// A `list-report`'s answer (`list.c`).
pub fn list(items: &[ListItem], room: usize) -> Result<Vec<u8>, DavRefusal> {
  let mut body = Body::new(room);
  body.push(XML_HEADER)?;
  body.push("<S:list-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n")?;
  for item in items {
    let size = item
      .size
      .map(|size| format!(" size=\"{size}\""))
      .unwrap_or_default();
    let props = item
      .has_props
      .map(|has| format!(" has-props=\"{has}\""))
      .unwrap_or_default();
    let created = item
      .created_rev
      .map(|revision| format!(" created-rev=\"{revision}\""))
      .unwrap_or_default();
    let date = match item.date {
      Some(date) => format!(" date=\"{}\"", quote(&svn_time(date)?, false)),
      None => String::new(),
    };
    let author = item
      .author
      .as_ref()
      .map(|author| {
        format!(
          "<D:creator-displayname>{}</D:creator-displayname>",
          quote(author, true)
        )
      })
      .unwrap_or_default();
    body.push(&format!(
      "<S:item node-kind=\"{}\"{size}{props}{created}{date}>{}{author}</S:item>\n",
      item.kind,
      quote(&item.path, false)
    ))?;
  }
  body.push("</S:list-report>\n")?;
  Ok(body.into_bytes())
}

/// One revision of a `file-revs-report`'s answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRev {
  /// The file's path in that revision.
  pub path: String,
  /// The revision.
  pub revision: Revision,
  /// Its revision properties.
  pub rev_props: Vec<(String, Vec<u8>)>,
  /// Its property changes: a value set, or `None` removed.
  pub props: Vec<(String, Option<Vec<u8>>)>,
  /// A revision merged in (`S:merged-revision`).
  pub merged: bool,
  /// Its text as a delta against the previous revision's, if it changed.
  pub delta: Option<Vec<Window>>,
}

/// A `file-revs-report`'s answer (`file-revs.c`), each delta written in `version`.
pub fn file_revs(
  revisions: &[FileRev],
  version: Version,
  room: usize,
) -> Result<Vec<u8>, DavRefusal> {
  let mut body = Body::new(room);
  body.push(FILE_REVS_START)?;
  for file_rev in revisions {
    body.push(&file_rev_element(file_rev, version)?)?;
  }
  body.push(FILE_REVS_END)?;
  Ok(body.into_bytes())
}

/// The start of a `file-revs-report`'s answer, `maybe_send_header`'s: the XML declaration and the report's element.
/// Format: `file-revs.c` `maybe_send_header` (Subversion 1.15.0), `DEBUG_CR` a line feed.
pub(crate) const FILE_REVS_START: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<S:file-revs-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n";

/// The end of a `file-revs-report`'s answer.
/// Format: `dav_svn__file_revs_report`'s last write (Subversion 1.15.0).
pub(crate) const FILE_REVS_END: &str = "</S:file-revs-report>\n";

/// One revision's `S:file-rev` element, as `file_rev_handler` and `delta_window_handler` write it (`file-revs.c`), its
/// delta written in `version`.
pub(crate) fn file_rev_element(file_rev: &FileRev, version: Version) -> Result<String, DavRefusal> {
  let mut element = format!(
    "<S:file-rev path=\"{}\" rev=\"{}\">\n",
    quote(&file_rev.path, true),
    file_rev.revision
  );
  for (name, value) in &file_rev.rev_props {
    element.push_str(&prop("rev-prop", name, value));
  }
  for (name, value) in &file_rev.props {
    element.push_str(&match value {
      Some(value) => prop("set-prop", name, value),
      None => format!("<S:remove-prop name=\"{}\"/>\n", quote(name, true)),
    });
  }
  if file_rev.merged {
    element.push_str("<S:merged-revision/>");
  }
  match &file_rev.delta {
    Some(windows) => element.push_str(&format!(
      "<S:txdelta>{}</S:txdelta></S:file-rev>\n",
      delta(windows, version)?
    )),
    None => element.push_str("</S:file-rev>\n"),
  }
  Ok(element)
}

/// One property as `send_prop` writes it: escaped as `svn_xml_escape_cdata_string` escapes, or base64 with line breaks.
fn prop(element: &str, name: &str, value: &[u8]) -> String {
  let name = quote(name, true);
  match std::str::from_utf8(value)
    .ok()
    .filter(|_| is_xml_safe(value))
  {
    Some(text) => format!(
      "<S:{element} name=\"{name}\">{}</S:{element}>\n",
      quote(text, false).replace('\r', "&#13;")
    ),
    None => format!(
      "<S:{element} name=\"{name}\" encoding=\"base64\">{}</S:{element}>\n",
      base64_lines(value)
    ),
  }
}

/// A delta's svndiff bytes in base64 with no line breaks, as the base64 output stream writes them.
pub(crate) fn delta(windows: &[Window], version: Version) -> Result<String, DavRefusal> {
  let mut encoder = Encoder::new(version);
  let mut bytes = encoder.header().to_vec();
  for window in windows {
    encoder
      .window(window, Compression::Stored, &mut bytes)
      .map_err(|error| DavRefusal::Malformed {
        element: "txdelta",
        text: format!("{error:?}"),
      })?;
  }
  Ok(base64_lines(&bytes).replace('\n', ""))
}

#[cfg(test)]
mod tests;
