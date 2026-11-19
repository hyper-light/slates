//! The bounded XML reader and the writer's escapes (§4.17.4 piece 13; A-609): a request body read whole, within the
//! caller's [`Bounds`], into a tree of elements with their namespaces resolved, as `mod_dav`'s parser (expat, through
//! `apr_xml_parse`) hands `mod_dav_svn` one.
//!
//! What it refuses, each by kind with the byte it stopped at ([`XmlRefusal`]): a body past its bound; bytes that are
//! not UTF-8, or a declaration naming another encoding; a `DOCTYPE`, so no entity is ever declared, and so none is ever
//! expanded, which leaves a billion laughs nothing to grow; a reference to any entity but XML's five, or to a character
//! XML 1.0 does not allow; a processing instruction past the XML declaration; a name past its bound, a nesting past its
//! depth, an element past its count, attributes past theirs, or one attribute twice; a prefix no declaration binds; an
//! end tag that closes another element; and any byte where the grammar allows none. A body that ends early is refused
//! as cut, wherever the cut falls.
//!
//! What it keeps as expat keeps it: a line break in text is one line feed (`\r\n` and `\r` alike, XML 1.0 §2.11); in an
//! attribute value a literal tab, line feed or carriage return is a space, and a character reference stays what it
//! names (§3.3.3); comments are dropped; a CDATA section is text. An element's text is the text directly in it, its
//! children's own text apart, as `dav_xml_get_cdata` joins it.
//!
//! The writer's side is the escapes `mod_dav_svn` writes with: `apr_xml_quote_string` ([`quote`]), the check
//! `svn_xml_is_xml_safe` makes before a value is sent as is ([`is_xml_safe`]), `svn_xml_fuzzy_escape` for a log message
//! ([`fuzzy_escape`]), and the base64 of `svn_base64_encode_string2` with its line breaks ([`base64_lines`]).
//!
//! No I/O, no clock, no randomness, no panic; the reader keeps its own stack, so a body's depth costs no recursion.

use std::borrow::Cow;

use super::Bounds;

/// The namespace the `xml` prefix is bound to without a declaration.
/// Format: Namespaces in XML 1.0 §3, "The prefix xml is by definition bound to the namespace name
/// <http://www.w3.org/XML/1998/namespace>".
const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// The attribute name that declares the default namespace, and the prefix that declares another.
/// Format: Namespaces in XML 1.0 §3, `xmlns` and `xmlns:`.
const XMLNS: &str = "xmlns";

/// The byte order mark a UTF-8 body may begin with.
/// Format: XML 1.0 §4.3.3, U+FEFF.
const BYTE_ORDER_MARK: &str = "\u{feff}";

/// The base64 alphabet `svn_base64_encode_string2` writes with, padding with `=`.
/// Format: RFC 4648 §4, table 1; `libsvn_subr/base64.c` `base64tab`.
const BASE64_ALPHABET: &[u8; 64] =
  b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The input bytes of one full base64 line, 76 characters of output.
/// Format: `libsvn_subr/base64.c` `BASE64_LINELEN` (76) and `BYTES_PER_LINE` (76 / 4 × 3 = 57).
const BASE64_BYTES_PER_LINE: usize = 57;

/// The input bytes one base64 group encodes, and the characters it writes.
/// Format: RFC 4648 §4, 24-bit groups of four 6-bit characters.
const BASE64_GROUP: (usize, usize) = (3, 4);

/// The bits one base64 character carries.
/// Format: RFC 4648 §4.
const BASE64_BITS: u32 = 6;

/// The mask of one base64 character's bits.
/// Format: RFC 4648 §4, 2^6 - 1.
const BASE64_MASK: u32 = 0x3f;

/// The control characters XML carries, which a value may hold and still be sent as is: tab, line feed, carriage
/// return and delete.
/// Format: `libsvn_subr/xml.c` `svn_xml_is_xml_safe`; `svn_ctype_iscntrl` is bytes 0 to 31 and 127 (`ctype.c`), the
/// same bytes as `u8::is_ascii_control`.
const XML_SAFE_CONTROLS: [u8; 4] = [b'\t', b'\n', b'\r', b'\x7f'];

/// The control characters `svn_xml_fuzzy_escape` keeps as they are.
/// Format: `libsvn_subr/xml.c` `svn_xml_fuzzy_escape`: line feed, carriage return and tab.
const FUZZY_KEPT_CONTROLS: [u8; 3] = [b'\n', b'\r', b'\t'];

/// The radix of a hexadecimal character reference, `&#x…;`.
/// Format: XML 1.0 §4.1.
const HEX: u32 = 16;

/// The radix of a decimal character reference, `&#…;`.
/// Format: XML 1.0 §4.1.
const DECIMAL: u32 = 10;

/// One element of a body: its namespace and local name, its attributes, and what it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
  /// The namespace its prefix, or the default declaration, binds it to; empty for none.
  pub namespace: String,
  /// Its local name.
  pub name: String,
  /// Its attributes, namespace declarations apart.
  pub attributes: Vec<Attribute>,
  /// What it holds, in order.
  pub children: Vec<Node>,
}

/// One attribute of an element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
  /// The namespace its prefix binds it to; empty for an unprefixed attribute, which no default declaration reaches.
  pub namespace: String,
  /// Its local name.
  pub name: String,
  /// Its value, references replaced and whitespace normalized.
  pub value: String,
}

/// One thing an element holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
  /// A child element.
  Element(Element),
  /// A run of text between child elements.
  Text(String),
}

impl Element {
  /// Whether the element is `name` in `namespace`.
  pub fn is(&self, namespace: &str, name: &str) -> bool {
    self.namespace == namespace && self.name == name
  }

  /// Its child elements, in order.
  pub fn elements(&self) -> impl Iterator<Item = &Element> {
    self.children.iter().filter_map(|node| match node {
      Node::Element(element) => Some(element),
      Node::Text(_) => None,
    })
  }

  /// Its text, the runs directly in it joined (`dav_xml_get_cdata` with `strip_white` 0).
  pub fn text(&self) -> String {
    self
      .children
      .iter()
      .filter_map(|node| match node {
        Node::Text(text) => Some(text.as_str()),
        Node::Element(_) => None,
      })
      .collect()
  }

  /// Its text with leading and trailing whitespace cut (`dav_xml_get_cdata` with `strip_white` 1, `apr_isspace`).
  pub fn trimmed_text(&self) -> String {
    self.text().trim_matches(is_apr_space).to_owned()
  }

  /// The value of its attribute named `name`, whatever its namespace, as `mod_dav_svn` compares `this_attr->name`.
  pub fn attribute(&self, name: &str) -> Option<&str> {
    self
      .attributes
      .iter()
      .find(|attribute| attribute.name == name)
      .map(|attribute| attribute.value.as_str())
  }
}

/// Whether `character` is whitespace as `apr_isspace` reads it: space, tab, line feed, vertical tab, form feed and
/// carriage return.
fn is_apr_space(character: char) -> bool {
  matches!(character, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r')
}

/// Why a body's XML was refused, each with the byte offset it stopped at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XmlRefusal {
  /// More bytes than the body's bound.
  TooLarge {
    /// The body's bytes.
    bytes: usize,
    /// The bound.
    bound: usize,
  },
  /// Bytes that are not UTF-8.
  NotUtf8 {
    /// Where.
    at: usize,
  },
  /// An XML declaration naming an encoding other than UTF-8.
  Encoding {
    /// Where.
    at: usize,
  },
  /// A document type declaration: no DTD is read, ever.
  Doctype {
    /// Where.
    at: usize,
  },
  /// A processing instruction other than the XML declaration.
  ProcessingInstruction {
    /// Where.
    at: usize,
  },
  /// A reference to an entity other than XML's five; with no DTD none can be declared.
  UnknownEntity {
    /// Where.
    at: usize,
  },
  /// A character reference naming no character XML 1.0 allows, or one past Unicode.
  BadCharacterReference {
    /// Where.
    at: usize,
  },
  /// A character XML 1.0 does not allow in a document.
  IllegalCharacter {
    /// Where.
    at: usize,
  },
  /// A name longer than its bound.
  NameTooLong {
    /// Where.
    at: usize,
  },
  /// Elements nested deeper than the bound.
  TooDeep {
    /// Where.
    at: usize,
  },
  /// More elements than the bound.
  TooManyElements {
    /// Where.
    at: usize,
  },
  /// More attributes on one element than the bound.
  TooManyAttributes {
    /// Where.
    at: usize,
  },
  /// One attribute twice on an element.
  DuplicateAttribute {
    /// Where.
    at: usize,
  },
  /// A prefix no declaration in scope binds.
  UnboundPrefix {
    /// Where.
    at: usize,
  },
  /// An end tag that closes an element other than the open one.
  MismatchedEnd {
    /// Where.
    at: usize,
  },
  /// The body ends inside a construct, or before its root element closes.
  Cut {
    /// Where.
    at: usize,
  },
  /// No root element.
  NoRoot,
  /// Content after the root element closed.
  TrailingContent {
    /// Where.
    at: usize,
  },
  /// A byte the grammar allows nowhere it stands.
  Syntax {
    /// Where.
    at: usize,
  },
}

impl XmlRefusal {
  /// Whether the body went past one of its bounds, rather than breaking the grammar.
  pub(crate) fn past_a_bound(&self) -> bool {
    matches!(
      self,
      Self::TooLarge { .. }
        | Self::NameTooLong { .. }
        | Self::TooDeep { .. }
        | Self::TooManyElements { .. }
        | Self::TooManyAttributes { .. }
    )
  }
}

/// Reads `body` whole into its root element, within `bounds`.
pub fn parse(body: &[u8], bounds: &Bounds) -> Result<Element, XmlRefusal> {
  if body.len() > bounds.body_bytes {
    return Err(XmlRefusal::TooLarge {
      bytes: body.len(),
      bound: bounds.body_bytes,
    });
  }
  let text = std::str::from_utf8(body).map_err(|error| XmlRefusal::NotUtf8 {
    at: error.valid_up_to(),
  })?;
  let mut reader = Reader {
    text,
    at: text
      .strip_prefix(BYTE_ORDER_MARK)
      .map_or(0, |_| BYTE_ORDER_MARK.len()),
    bounds,
    elements: 0,
    scopes: vec![(String::from("xml"), String::from(XML_NAMESPACE))],
  };
  reader.declaration()?;
  reader.misc()?;
  if reader.rest().is_empty() {
    return Err(XmlRefusal::NoRoot);
  }
  let root = reader.root()?;
  reader.misc()?;
  if reader.rest().is_empty() {
    Ok(root)
  } else {
    Err(XmlRefusal::TrailingContent { at: reader.at })
  }
}

/// An element open while its content is read.
struct Open {
  /// The name as written, prefix and all, which its end tag must repeat.
  written: String,
  /// The element so far.
  element: Element,
  /// How many namespace bindings were in scope before it.
  scopes: usize,
}

/// The reader's place in a body.
struct Reader<'b> {
  /// The body, UTF-8.
  text: &'b str,
  /// The byte it reads next.
  at: usize,
  /// The caller's bounds.
  bounds: &'b Bounds,
  /// The elements read so far.
  elements: usize,
  /// The namespace bindings in scope, innermost last: each a prefix (empty for the default) and its namespace.
  scopes: Vec<(String, String)>,
}

impl Reader<'_> {
  /// What is left to read.
  fn rest(&self) -> &str {
    self.text.get(self.at..).unwrap_or("")
  }

  /// Moves past `bytes` bytes.
  fn advance(&mut self, bytes: usize) {
    self.at = self.at.saturating_add(bytes).min(self.text.len());
  }

  /// Moves past `literal` if the rest begins with it.
  fn eat(&mut self, literal: &str) -> bool {
    let starts = self.rest().starts_with(literal);
    if starts {
      self.advance(literal.len());
    }
    starts
  }

  /// Moves past whitespace (XML 1.0 §2.3 `S`).
  fn skip_space(&mut self) {
    let rest = self.rest();
    let kept = rest.trim_start_matches([' ', '\t', '\r', '\n']).len();
    self.advance(rest.len().saturating_sub(kept));
  }

  /// The XML declaration, if the body begins with one; refused unless its encoding, if named, is UTF-8.
  fn declaration(&mut self) -> Result<(), XmlRefusal> {
    let declared = self
      .rest()
      .strip_prefix("<?xml")
      .is_some_and(|after| after.starts_with([' ', '\t', '\r', '\n']));
    if !declared {
      return Ok(());
    }
    let start = self.at;
    let end = self.rest().find("?>").ok_or(XmlRefusal::Cut {
      at: self.text.len(),
    })?;
    let inside = self.rest().get("<?xml".len()..end).unwrap_or("");
    let utf8 = declared_encoding(inside).is_none_or(|encoding| {
      encoding.eq_ignore_ascii_case("utf-8") || encoding.eq_ignore_ascii_case("utf8")
    });
    if !inside.trim_start().starts_with("version") {
      return Err(XmlRefusal::Syntax { at: start });
    }
    if !utf8 {
      return Err(XmlRefusal::Encoding { at: start });
    }
    self.advance(end.saturating_add("?>".len()));
    Ok(())
  }

  /// Whitespace and comments before or after the root; a processing instruction or a DTD is refused.
  fn misc(&mut self) -> Result<(), XmlRefusal> {
    loop {
      self.skip_space();
      if self.rest().starts_with("<!--") {
        self.comment()?;
      } else if self.rest().starts_with("<!DOCTYPE") {
        return Err(XmlRefusal::Doctype { at: self.at });
      } else if self.rest().starts_with("<?") {
        return Err(XmlRefusal::ProcessingInstruction { at: self.at });
      } else {
        return Ok(());
      }
    }
  }

  /// A comment, dropped; `--` inside one is refused, as XML 1.0 §2.5 refuses it.
  fn comment(&mut self) -> Result<(), XmlRefusal> {
    let start = self.at;
    self.advance("<!--".len());
    let end = self.rest().find("--").ok_or(XmlRefusal::Cut {
      at: self.text.len(),
    })?;
    let body = self.rest().get(..end).unwrap_or("");
    check_characters(body, self.at)?;
    self.advance(end.saturating_add("--".len()));
    if self.eat(">") {
      Ok(())
    } else if self.rest().is_empty() {
      Err(XmlRefusal::Cut { at: self.at })
    } else {
      Err(XmlRefusal::Syntax { at: start })
    }
  }

  /// The root element and everything in it, read with a stack of open elements.
  fn root(&mut self) -> Result<Element, XmlRefusal> {
    let mut open: Vec<Open> = Vec::new();
    loop {
      if self.rest().is_empty() {
        return Err(XmlRefusal::Cut { at: self.at });
      }
      if self.rest().starts_with("</") {
        let closed = self.end_tag(&mut open)?;
        match open.last_mut() {
          Some(parent) => parent.element.children.push(Node::Element(closed)),
          None => return Ok(closed),
        }
      } else if self.rest().starts_with('<') {
        if let Some(done) = self.markup(&mut open)? {
          return Ok(done);
        }
      } else {
        self.text_run(&mut open)?;
      }
    }
  }

  /// A comment, a CDATA section, or a start tag: an empty element at the root ends the body's root.
  fn markup(&mut self, open: &mut Vec<Open>) -> Result<Option<Element>, XmlRefusal> {
    if self.rest().starts_with("<!--") {
      self.comment()?;
      return Ok(None);
    }
    if self.rest().starts_with("<![CDATA[") {
      let text = self.cdata()?;
      push_text(open, &text, self.at)?;
      return Ok(None);
    }
    if self.rest().starts_with("<!") {
      return Err(XmlRefusal::Doctype { at: self.at });
    }
    if self.rest().starts_with("<?") {
      return Err(XmlRefusal::ProcessingInstruction { at: self.at });
    }
    let (started, empty) = self.start_tag(open.len())?;
    if !empty {
      open.push(started);
      return Ok(None);
    }
    self.scopes.truncate(started.scopes);
    match open.last_mut() {
      Some(parent) => {
        parent.element.children.push(Node::Element(started.element));
        Ok(None)
      }
      None => Ok(Some(started.element)),
    }
  }

  /// A start tag: its element, and whether it closed itself.
  fn start_tag(&mut self, depth: usize) -> Result<(Open, bool), XmlRefusal> {
    let start = self.at;
    self.advance(1);
    if depth >= self.bounds.depth {
      return Err(XmlRefusal::TooDeep { at: start });
    }
    self.elements = self.elements.saturating_add(1);
    if self.elements > self.bounds.elements {
      return Err(XmlRefusal::TooManyElements { at: start });
    }
    let written = self.name()?;
    let raw = self.attributes()?;
    let empty = self.eat("/>");
    if !empty && !self.eat(">") {
      return Err(self.cut_or(XmlRefusal::Syntax { at: self.at }));
    }
    let scopes = self.scopes.len();
    self.declare(&raw);
    let element = self.resolve(&written, raw, start)?;
    Ok((
      Open {
        written,
        element,
        scopes,
      },
      empty,
    ))
  }

  /// The refusal for a construct the body ends inside, or `otherwise`.
  fn cut_or(&self, otherwise: XmlRefusal) -> XmlRefusal {
    if self.rest().is_empty() {
      XmlRefusal::Cut { at: self.at }
    } else {
      otherwise
    }
  }

  /// A name (XML 1.0 §2.3 `Name`, the bytes a name may hold), within the bound.
  fn name(&mut self) -> Result<String, XmlRefusal> {
    let start = self.at;
    let rest = self.rest();
    let length = rest
      .char_indices()
      .find(|(_, character)| !is_name_character(*character))
      .map_or(rest.len(), |(at, _)| at);
    let name = rest.get(..length).unwrap_or("");
    let first_ok = name.chars().next().is_some_and(is_name_start);
    if !first_ok {
      return Err(self.cut_or(XmlRefusal::Syntax { at: start }));
    }
    if name.len() > self.bounds.name_bytes {
      return Err(XmlRefusal::NameTooLong { at: start });
    }
    let name = name.to_owned();
    self.advance(length);
    Ok(name)
  }

  /// A start tag's attributes, as written, within the bound and each once.
  fn attributes(&mut self) -> Result<Vec<(String, String)>, XmlRefusal> {
    let mut raw: Vec<(String, String)> = Vec::new();
    loop {
      let before = self.at;
      self.skip_space();
      if self.rest().starts_with('>') || self.rest().starts_with("/>") || self.rest().is_empty() {
        return Ok(raw);
      }
      if self.at == before {
        return Err(XmlRefusal::Syntax { at: self.at });
      }
      let at = self.at;
      let name = self.name()?;
      self.skip_space();
      if !self.eat("=") {
        return Err(self.cut_or(XmlRefusal::Syntax { at: self.at }));
      }
      self.skip_space();
      let value = self.attribute_value()?;
      if raw.iter().any(|(seen, _)| *seen == name) {
        return Err(XmlRefusal::DuplicateAttribute { at });
      }
      if raw.len() >= self.bounds.attributes {
        return Err(XmlRefusal::TooManyAttributes { at });
      }
      raw.push((name, value));
    }
  }

  /// A quoted attribute value: references replaced, and each literal tab, line feed or carriage return a space, a
  /// carriage return and line feed one space (XML 1.0 §3.3.3).
  fn attribute_value(&mut self) -> Result<String, XmlRefusal> {
    let quote = match self.rest().chars().next() {
      Some(quote @ ('"' | '\'')) => quote,
      _ => return Err(self.cut_or(XmlRefusal::Syntax { at: self.at })),
    };
    self.advance(1);
    let end = self.rest().find(quote).ok_or(XmlRefusal::Cut {
      at: self.text.len(),
    })?;
    let start = self.at;
    let raw = self.rest().get(..end).unwrap_or("");
    if let Some(less) = raw.find('<') {
      return Err(XmlRefusal::Syntax {
        at: start.saturating_add(less),
      });
    }
    let normalized = raw.replace("\r\n", " ").replace(['\t', '\n', '\r'], " ");
    let value = replace_references(&normalized, start)?;
    self.advance(end.saturating_add(1));
    Ok(value)
  }

  /// Puts the namespace declarations among `raw` in scope.
  fn declare(&mut self, raw: &[(String, String)]) {
    for (name, value) in raw {
      if name == XMLNS {
        self.scopes.push((String::new(), value.clone()));
      } else if let Some(prefix) = name.strip_prefix("xmlns:") {
        self.scopes.push((prefix.to_owned(), value.clone()));
      }
    }
  }

  /// The namespace `prefix` is bound to; the empty prefix of an element is the default namespace.
  fn bound(&self, prefix: &str) -> Option<&str> {
    self
      .scopes
      .iter()
      .rev()
      .find(|(declared, _)| declared == prefix)
      .map(|(_, namespace)| namespace.as_str())
  }

  /// The element a start tag wrote, its name and attributes resolved against the bindings in scope.
  fn resolve(
    &self,
    written: &str,
    raw: Vec<(String, String)>,
    at: usize,
  ) -> Result<Element, XmlRefusal> {
    let (namespace, name) = match written.split_once(':') {
      Some((prefix, local)) => (
        self.bound(prefix).ok_or(XmlRefusal::UnboundPrefix { at })?,
        local,
      ),
      None => (self.bound("").unwrap_or(""), written),
    };
    let mut attributes = Vec::with_capacity(raw.len());
    for (attribute, value) in raw {
      if attribute == XMLNS || attribute.starts_with("xmlns:") {
        continue;
      }
      let (namespace, local) = match attribute.split_once(':') {
        Some((prefix, local)) => (
          self.bound(prefix).ok_or(XmlRefusal::UnboundPrefix { at })?,
          local,
        ),
        None => ("", attribute.as_str()),
      };
      attributes.push(Attribute {
        namespace: namespace.to_owned(),
        name: local.to_owned(),
        value,
      });
    }
    Ok(Element {
      namespace: namespace.to_owned(),
      name: name.to_owned(),
      attributes,
      children: Vec::new(),
    })
  }

  /// An end tag, which must close the open element: that element, done.
  fn end_tag(&mut self, open: &mut Vec<Open>) -> Result<Element, XmlRefusal> {
    let at = self.at;
    self.advance("</".len());
    let name = self.name()?;
    self.skip_space();
    if !self.eat(">") {
      return Err(self.cut_or(XmlRefusal::Syntax { at: self.at }));
    }
    let closed = open.pop().ok_or(XmlRefusal::MismatchedEnd { at })?;
    if closed.written != name {
      return Err(XmlRefusal::MismatchedEnd { at });
    }
    self.scopes.truncate(closed.scopes);
    Ok(closed.element)
  }

  /// A CDATA section's text.
  fn cdata(&mut self) -> Result<String, XmlRefusal> {
    self.advance("<![CDATA[".len());
    let end = self.rest().find("]]>").ok_or(XmlRefusal::Cut {
      at: self.text.len(),
    })?;
    let body = self.rest().get(..end).unwrap_or("");
    check_characters(body, self.at)?;
    let text = normalize_line_breaks(body);
    self.advance(end.saturating_add("]]>".len()));
    Ok(text)
  }

  /// Text up to the next markup, in the open element.
  fn text_run(&mut self, open: &mut [Open]) -> Result<(), XmlRefusal> {
    let start = self.at;
    let end = self.rest().find('<').unwrap_or(self.rest().len());
    let raw = self.rest().get(..end).unwrap_or("");
    if raw.contains("]]>") {
      return Err(XmlRefusal::Syntax { at: start });
    }
    let text = replace_references(&normalize_line_breaks(raw), start)?;
    self.advance(end);
    push_text(open, &text, start)
  }
}

/// Adds `text` to the open element's last run of text, or starts one; text outside the root is refused unless it is
/// whitespace, which the reader skips before it gets here.
fn push_text(open: &mut [Open], text: &str, at: usize) -> Result<(), XmlRefusal> {
  let Some(parent) = open.last_mut() else {
    return Err(XmlRefusal::Syntax { at });
  };
  match parent.element.children.last_mut() {
    Some(Node::Text(run)) => run.push_str(text),
    _ => parent.element.children.push(Node::Text(text.to_owned())),
  }
  Ok(())
}

/// The encoding an XML declaration's inside names, if it names one.
fn declared_encoding(inside: &str) -> Option<&str> {
  let after = inside.split_once("encoding")?.1.trim_start();
  let after = after.strip_prefix('=')?.trim_start();
  let quote = after.chars().next()?;
  let value = after.get(quote.len_utf8()..)?;
  value.split_once(quote).map(|(encoding, _)| encoding)
}

/// Whether `character` may start a name (XML 1.0 §2.3 `NameStartChar`).
fn is_name_start(character: char) -> bool {
  character.is_ascii_alphabetic() || character == '_' || character == ':' || !character.is_ascii()
}

/// Whether `character` may stand in a name (XML 1.0 §2.3 `NameChar`).
fn is_name_character(character: char) -> bool {
  is_name_start(character) || character.is_ascii_digit() || character == '-' || character == '.'
}

/// Whether `character` is one XML 1.0 allows in a document (§2.2 `Char`).
fn is_xml_character(character: char) -> bool {
  matches!(character, '\t' | '\n' | '\r')
    || (character >= ' ' && !matches!(character, '\u{fffe}' | '\u{ffff}'))
}

/// Refuses the first character of `text` XML 1.0 does not allow, `text` starting at byte `at`.
fn check_characters(text: &str, at: usize) -> Result<(), XmlRefusal> {
  match text
    .char_indices()
    .find(|(_, character)| !is_xml_character(*character))
  {
    Some((offset, _)) => Err(XmlRefusal::IllegalCharacter {
      at: at.saturating_add(offset),
    }),
    None => Ok(()),
  }
}

/// `text` with each carriage return and line feed, and each lone carriage return, one line feed (XML 1.0 §2.11).
fn normalize_line_breaks(text: &str) -> String {
  text.replace("\r\n", "\n").replace('\r', "\n")
}

/// `text` (starting at byte `at`) with its references replaced: XML's five entities and character references; any
/// other entity is refused, as is a character XML does not allow, raw or referenced.
fn replace_references(text: &str, at: usize) -> Result<String, XmlRefusal> {
  check_characters(text, at)?;
  let mut out = String::with_capacity(text.len());
  let mut rest = text;
  while let Some(amp) = rest.find('&') {
    out.push_str(rest.get(..amp).unwrap_or(""));
    let here = at
      .saturating_add(text.len().saturating_sub(rest.len()))
      .saturating_add(amp);
    let after = rest.get(amp.saturating_add(1)..).unwrap_or("");
    let semicolon = after
      .find(';')
      .ok_or(XmlRefusal::UnknownEntity { at: here })?;
    let entity = after.get(..semicolon).unwrap_or("");
    out.push(referenced(entity, here)?);
    rest = after.get(semicolon.saturating_add(1)..).unwrap_or("");
  }
  out.push_str(rest);
  Ok(out)
}

/// The character `&entity;` names.
fn referenced(entity: &str, at: usize) -> Result<char, XmlRefusal> {
  let named = match entity {
    "lt" => Some('<'),
    "gt" => Some('>'),
    "amp" => Some('&'),
    "quot" => Some('"'),
    "apos" => Some('\''),
    _ => None,
  };
  if let Some(character) = named {
    return Ok(character);
  }
  let number = entity
    .strip_prefix('#')
    .ok_or(XmlRefusal::UnknownEntity { at })?;
  let (digits, radix) = match number.strip_prefix('x') {
    Some(hex) => (hex, HEX),
    None => (number, DECIMAL),
  };
  let valid_digits = !digits.is_empty() && digits.chars().all(|digit| digit.is_digit(radix));
  let code = if valid_digits {
    u32::from_str_radix(digits, radix).ok()
  } else {
    None
  };
  code
    .and_then(char::from_u32)
    .filter(|character| is_xml_character(*character))
    .ok_or(XmlRefusal::BadCharacterReference { at })
}

/// `text` as `apr_xml_quote_string` writes it: `&`, `<` and `>` escaped, and `"` too when `quotes`.
pub fn quote(text: &str, quotes: bool) -> Cow<'_, str> {
  let needs = text
    .chars()
    .any(|character| matches!(character, '&' | '<' | '>') || (quotes && character == '"'));
  if !needs {
    return Cow::Borrowed(text);
  }
  let mut out = String::with_capacity(text.len());
  for character in text.chars() {
    match character {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' if quotes => out.push_str("&quot;"),
      other => out.push(other),
    }
  }
  Cow::Owned(out)
}

/// Whether `bytes` may be sent in XML as they are (`svn_xml_is_xml_safe`): UTF-8 with no control character but tab,
/// line feed, carriage return and delete.
pub fn is_xml_safe(bytes: &[u8]) -> bool {
  std::str::from_utf8(bytes).is_ok()
    && bytes
      .iter()
      .all(|byte| !byte.is_ascii_control() || XML_SAFE_CONTROLS.contains(byte))
}

/// `text` as `svn_xml_fuzzy_escape` writes it: each control character but line feed, carriage return and tab replaced
/// by `?\` and its three-digit decimal value.
pub fn fuzzy_escape(text: &str) -> Cow<'_, str> {
  let fuzzed = |byte: &u8| byte.is_ascii_control() && !FUZZY_KEPT_CONTROLS.contains(byte);
  if !text.as_bytes().iter().any(fuzzed) {
    return Cow::Borrowed(text);
  }
  let mut out = String::with_capacity(text.len());
  for character in text.chars() {
    let ascii = u8::try_from(u32::from(character)).ok();
    match ascii.filter(fuzzed) {
      Some(byte) => out.push_str(&format!("?\\{byte:03}")),
      None => out.push(character),
    }
  }
  Cow::Owned(out)
}

/// `bytes` in base64 as `svn_base64_encode_string2` writes them with line breaks: lines of 76 characters, each line,
/// the last one too, ended by a line feed; nothing for nothing.
pub fn base64_lines(bytes: &[u8]) -> String {
  let (group_bytes, _) = BASE64_GROUP;
  let mut out = String::with_capacity(bytes.len().saturating_mul(2));
  for line in bytes.chunks(BASE64_BYTES_PER_LINE) {
    for group in line.chunks(group_bytes) {
      push_base64_group(&mut out, group);
    }
    out.push('\n');
  }
  out
}

/// One group of up to three bytes in base64, padded.
fn push_base64_group(out: &mut String, group: &[u8]) {
  let (group_bytes, group_characters) = BASE64_GROUP;
  let missing = group_bytes.saturating_sub(group.len());
  let padded = group.iter().copied().chain(std::iter::repeat_n(0, missing));
  let word = padded.fold(0_u32, |word, byte| {
    word.checked_shl(u8::BITS).unwrap_or(0) | u32::from(byte)
  });
  let written = group_characters.saturating_sub(missing);
  for index in 0..group_characters {
    if index >= written {
      out.push('=');
      continue;
    }
    let place =
      u32::try_from(group_characters.saturating_sub(index).saturating_sub(1)).unwrap_or(0);
    let digit = word
      .checked_shr(BASE64_BITS.saturating_mul(place))
      .unwrap_or(0)
      & BASE64_MASK;
    let digit = usize::try_from(digit).unwrap_or(0);
    out.push(
      BASE64_ALPHABET
        .get(digit)
        .map_or('=', |byte| char::from(*byte)),
    );
  }
}
