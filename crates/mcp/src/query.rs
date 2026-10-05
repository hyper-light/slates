//! Codemode (§4.12, condition 13): `slates.query`, one read-only query an agent writes in place of many tool calls,
//! run in the MCP server over the client's verbs, with only its answer returned. vorpal serves its graph the same
//! way (a Cypher-shaped read-only language, `vorpal-query`); Anthropic measured the pattern of composing calls
//! server-side and returning only the result at 98.7% fewer tokens in one worked example and 37% fewer on complex
//! research tasks (`docs/wip/research/mcp-skills-sdks.md` §2.2.5).
//!
//! ```text
//! FROM files("<volume>", under = "src") WHERE ext = "rs" AND content CONTAINS "unsafe"
//! SELECT path, size ORDER BY size DESC LIMIT 20
//! ```
//!
//! Sources: `volumes()` (id, name, referenced, unique); `files(VOL [, version = N] [, under = "dir"])` (path, name,
//! ext, dir, kind, size, content: every entry below `under`, walked); `lines(VOL [, version = N] [, under = "dir"])`
//! (path, line, text: every line of every file); `changed(VOL, since = N)` (path). A volume is its hex id or its
//! name. Conditions: `=`, `!=`/`<>`, `<`, `<=`, `>`, `>=`, `CONTAINS`, `STARTS WITH`, `ENDS WITH`, `GLOB` (`*` within
//! a path component, `**` across them), `AND`, `OR`, `NOT`, parentheses. `SELECT` defaults to every column but
//! `content`.
//!
//! It is structurally read-only: no construct changes anything. Every query runs under ceilings counted in work,
//! never wall time, and one exceeded is a typed refusal naming it, never a truncated answer:
//! - [`MAX_READ_BYTES`]: file bytes read (by `content` and `lines`);
//! - [`MAX_VISITS`]: entries visited by a walk;
//! - [`MAX_OUTPUT_BYTES`]: the encoded answer.
//!
//! Without `ORDER BY`, a `LIMIT` stops the walk once it has its rows.

use std::collections::BTreeMap;

use slates_client::{EntryKind, ReadAt, VolumeId};

/// Derived: the most file bytes one query reads: one MCP message's worth ([`crate::MAX_MESSAGE_BYTES`]), the bound a
/// single answer could carry anyway.
pub const MAX_READ_BYTES: u64 = crate::MAX_MESSAGE_BYTES;

/// Derived: the most directory entries one query's walk visits: one message's worth of the smallest listing entry
/// (a one-byte name, its four-byte length, a kind tag and an eight-byte size: fourteen bytes).
pub const MAX_VISITS: u64 = crate::MAX_MESSAGE_BYTES / 14;

/// Shape: the most bytes a query's answer may take when encoded: Claude Code's default limit on one MCP tool's
/// output, 25,000 tokens (`MAX_MCP_OUTPUT_TOKENS`, research/mcp-skills-sdks.md §2.1.2), at four bytes a token.
pub const MAX_OUTPUT_BYTES: usize = 25_000 * 4;

/// What a query reads, through the client's verbs (a trait so the language is tested against an in-memory volume).
pub trait Source {
  /// Every volume: id (hex), name, referenced and unique bytes.
  fn volumes(&mut self) -> Result<Vec<(VolumeId, String, u64, u64)>, String>;
  /// A directory's direct entries at a view: name, kind, size.
  fn list(
    &mut self,
    volume: VolumeId,
    path: &str,
    at: ReadAt,
  ) -> Result<Vec<(String, EntryKind, u64)>, String>;
  /// A file's bytes at a view.
  fn read(&mut self, volume: VolumeId, path: &str, at: ReadAt) -> Result<Vec<u8>, String>;
  /// The paths a green changed after `since`.
  fn changed(&mut self, green: VolumeId, since: u64) -> Result<Vec<String>, String>;
}

/// Why a query was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryError {
  /// The text is not a query of this language.
  Syntax(String),
  /// A name the query uses is unknown (a column, a source, a volume).
  Unknown(String),
  /// A ceiling was reached: its name and its limit.
  Ceiling {
    /// The ceiling.
    name: &'static str,
    /// Its limit.
    limit: u64,
  },
  /// The daemon refused a read the query needed.
  Refused(String),
}

impl std::fmt::Display for QueryError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      QueryError::Syntax(why) => write!(f, "syntax: {why}"),
      QueryError::Unknown(name) => write!(f, "unknown: {name}"),
      QueryError::Ceiling { name, limit } => write!(
        f,
        "ceiling {name} ({limit}) reached: narrow the query (WHERE, under =, SELECT fewer columns, LIMIT)"
      ),
      QueryError::Refused(why) => write!(f, "refused: {why}"),
    }
  }
}

/// A value in a row.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cell {
  /// No value.
  Null,
  /// A truth value.
  Bool(bool),
  /// A number.
  Int(u64),
  /// Text.
  Text(String),
}

impl Cell {
  fn text(&self) -> String {
    match self {
      Cell::Null => String::new(),
      Cell::Bool(value) => value.to_string(),
      Cell::Int(value) => value.to_string(),
      Cell::Text(value) => value.clone(),
    }
  }

  fn truthy(&self) -> bool {
    match self {
      Cell::Null => false,
      Cell::Bool(value) => *value,
      Cell::Int(value) => *value != 0,
      Cell::Text(value) => !value.is_empty(),
    }
  }

  /// The cell as JSON.
  pub fn to_json(&self) -> serde_json::Value {
    match self {
      Cell::Null => serde_json::Value::Null,
      Cell::Bool(value) => serde_json::Value::Bool(*value),
      Cell::Int(value) => serde_json::Value::from(*value),
      Cell::Text(value) => serde_json::Value::String(value.clone()),
    }
  }
}

/// A query's answer: its columns, its rows, how many rows matched before `LIMIT`, and the work it did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
  /// The columns, in order.
  pub columns: Vec<String>,
  /// The rows.
  pub rows: Vec<Vec<Cell>>,
  /// The rows that matched before `LIMIT` (equal to `rows.len()` when the walk stopped early at it).
  pub matched: u64,
  /// Entries visited.
  pub visited: u64,
  /// File bytes read.
  pub bytes_read: u64,
}

// ------------------------------------------------------------------------------------------------- lexing

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
  Word(String),
  Text(String),
  Int(u64),
  Symbol(&'static str),
}

/// Format: the symbols of the language, longest first so `<=` is never read as `<` then `=`.
const SYMBOLS: &[&str] = &["<=", ">=", "!=", "<>", "(", ")", ",", "=", "<", ">"];

fn lex(text: &str) -> Result<Vec<Token>, QueryError> {
  let mut tokens = Vec::new();
  let mut rest = text.trim_start();
  while !rest.is_empty() {
    if let Some(after) = rest.strip_prefix('"') {
      let end = after
        .find('"')
        .ok_or_else(|| QueryError::Syntax("an unterminated string".to_owned()))?;
      tokens.push(Token::Text(after.get(..end).unwrap_or_default().to_owned()));
      rest = after.get(end.saturating_add(1)..).unwrap_or_default();
    } else if let Some(symbol) = SYMBOLS.iter().find(|symbol| rest.starts_with(**symbol)) {
      tokens.push(Token::Symbol(symbol));
      rest = rest.get(symbol.len()..).unwrap_or_default();
    } else if rest.starts_with(|c: char| c.is_ascii_digit()) {
      let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
      let digits = rest.get(..end).unwrap_or_default();
      let value = digits
        .parse()
        .map_err(|_| QueryError::Syntax(format!("a number too large: {digits}")))?;
      tokens.push(Token::Int(value));
      rest = rest.get(end..).unwrap_or_default();
    } else if rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
      let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
      tokens.push(Token::Word(rest.get(..end).unwrap_or_default().to_owned()));
      rest = rest.get(end..).unwrap_or_default();
    } else {
      let shown: String = rest.chars().take(1).collect();
      return Err(QueryError::Syntax(format!(
        "an unexpected character: {shown}"
      )));
    }
    rest = rest.trim_start();
  }
  Ok(tokens)
}

// ------------------------------------------------------------------------------------------------- parsing

/// A parsed query.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Query {
  from: From,
  filter: Option<Expr>,
  select: Option<Vec<String>>,
  order: Vec<(String, bool)>,
  limit: Option<u64>,
}

/// A query's source.
#[derive(Clone, Debug, PartialEq, Eq)]
enum From {
  Volumes,
  Files {
    volume: String,
    version: Option<u64>,
    under: String,
  },
  Lines {
    volume: String,
    version: Option<u64>,
    under: String,
  },
  Changed {
    volume: String,
    since: u64,
  },
}

/// A condition or value.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Expr {
  Column(String),
  Literal(Cell),
  Not(Box<Expr>),
  And(Box<Expr>, Box<Expr>),
  Or(Box<Expr>, Box<Expr>),
  Compare(Box<Expr>, Op, Box<Expr>),
}

/// A comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
  Eq,
  Ne,
  Lt,
  Le,
  Gt,
  Ge,
  Contains,
  StartsWith,
  EndsWith,
  Glob,
}

struct Parser {
  tokens: Vec<Token>,
  at: usize,
}

impl Parser {
  fn peek(&self) -> Option<&Token> {
    self.tokens.get(self.at)
  }

  fn next(&mut self) -> Option<Token> {
    let token = self.tokens.get(self.at).cloned();
    self.at = self.at.saturating_add(1);
    token
  }

  fn keyword(&mut self, word: &str) -> bool {
    if matches!(self.peek(), Some(Token::Word(w)) if w.eq_ignore_ascii_case(word)) {
      self.at = self.at.saturating_add(1);
      return true;
    }
    false
  }

  fn expect_keyword(&mut self, word: &str) -> Result<(), QueryError> {
    if self.keyword(word) {
      Ok(())
    } else {
      Err(QueryError::Syntax(format!("expected {word}")))
    }
  }

  fn symbol(&mut self, symbol: &str) -> bool {
    if matches!(self.peek(), Some(Token::Symbol(s)) if *s == symbol) {
      self.at = self.at.saturating_add(1);
      return true;
    }
    false
  }

  fn expect_symbol(&mut self, symbol: &str) -> Result<(), QueryError> {
    if self.symbol(symbol) {
      Ok(())
    } else {
      Err(QueryError::Syntax(format!("expected {symbol}")))
    }
  }

  fn word(&mut self) -> Result<String, QueryError> {
    match self.next() {
      Some(Token::Word(word)) => Ok(word),
      other => Err(QueryError::Syntax(format!(
        "expected a name, found {other:?}"
      ))),
    }
  }

  fn query(&mut self) -> Result<Query, QueryError> {
    self.expect_keyword("FROM")?;
    let from = self.source()?;
    let filter = if self.keyword("WHERE") {
      Some(self.or()?)
    } else {
      None
    };
    let select = if self.keyword("SELECT") {
      let mut columns = vec![self.word()?.to_ascii_lowercase()];
      while self.symbol(",") {
        columns.push(self.word()?.to_ascii_lowercase());
      }
      Some(columns)
    } else {
      None
    };
    let mut order = Vec::new();
    if self.keyword("ORDER") {
      self.expect_keyword("BY")?;
      loop {
        let column = self.word()?.to_ascii_lowercase();
        let descending = if self.keyword("DESC") {
          true
        } else {
          self.keyword("ASC");
          false
        };
        order.push((column, descending));
        if !self.symbol(",") {
          break;
        }
      }
    }
    let limit = if self.keyword("LIMIT") {
      match self.next() {
        Some(Token::Int(limit)) => Some(limit),
        other => {
          return Err(QueryError::Syntax(format!(
            "LIMIT takes a number, found {other:?}"
          )));
        }
      }
    } else {
      None
    };
    if let Some(extra) = self.peek() {
      return Err(QueryError::Syntax(format!(
        "unexpected {extra:?} after the query"
      )));
    }
    Ok(Query {
      from,
      filter,
      select,
      order,
      limit,
    })
  }

  fn source(&mut self) -> Result<From, QueryError> {
    let name = self.word()?.to_ascii_lowercase();
    self.expect_symbol("(")?;
    if name == "volumes" {
      self.expect_symbol(")")?;
      return Ok(From::Volumes);
    }
    let volume = match self.next() {
      Some(Token::Text(volume)) => volume,
      other => {
        return Err(QueryError::Syntax(format!(
          "{name} takes a volume string, found {other:?}"
        )));
      }
    };
    let mut named: BTreeMap<String, Token> = BTreeMap::new();
    while self.symbol(",") {
      let key = self.word()?.to_ascii_lowercase();
      self.expect_symbol("=")?;
      let value = self
        .next()
        .ok_or_else(|| QueryError::Syntax(format!("{key} takes a value")))?;
      named.insert(key, value);
    }
    self.expect_symbol(")")?;
    let number = |named: &BTreeMap<String, Token>, key: &str| match named.get(key) {
      None => Ok(None),
      Some(Token::Int(value)) => Ok(Some(*value)),
      Some(other) => Err(QueryError::Syntax(format!(
        "{key} takes a number, found {other:?}"
      ))),
    };
    let under = match named.get("under") {
      None => String::new(),
      Some(Token::Text(under)) => under.trim_matches('/').to_owned(),
      Some(other) => {
        return Err(QueryError::Syntax(format!(
          "under takes a string, found {other:?}"
        )));
      }
    };
    let version = number(&named, "version")?;
    match name.as_str() {
      "files" => Ok(From::Files {
        volume,
        version,
        under,
      }),
      "lines" => Ok(From::Lines {
        volume,
        version,
        under,
      }),
      "changed" => Ok(From::Changed {
        volume,
        since: number(&named, "since")?
          .ok_or_else(|| QueryError::Syntax("changed takes since = N".to_owned()))?,
      }),
      other => Err(QueryError::Unknown(format!("source {other}"))),
    }
  }

  fn or(&mut self) -> Result<Expr, QueryError> {
    let mut left = self.and()?;
    while self.keyword("OR") {
      left = Expr::Or(Box::new(left), Box::new(self.and()?));
    }
    Ok(left)
  }

  fn and(&mut self) -> Result<Expr, QueryError> {
    let mut left = self.not()?;
    while self.keyword("AND") {
      left = Expr::And(Box::new(left), Box::new(self.not()?));
    }
    Ok(left)
  }

  fn not(&mut self) -> Result<Expr, QueryError> {
    if self.keyword("NOT") {
      return Ok(Expr::Not(Box::new(self.not()?)));
    }
    self.compare()
  }

  fn compare(&mut self) -> Result<Expr, QueryError> {
    let left = self.operand()?;
    let op = if self.symbol("=") {
      Op::Eq
    } else if self.symbol("!=") || self.symbol("<>") {
      Op::Ne
    } else if self.symbol("<=") {
      Op::Le
    } else if self.symbol(">=") {
      Op::Ge
    } else if self.symbol("<") {
      Op::Lt
    } else if self.symbol(">") {
      Op::Gt
    } else if self.keyword("CONTAINS") {
      Op::Contains
    } else if self.keyword("GLOB") {
      Op::Glob
    } else if self.keyword("STARTS") {
      self.expect_keyword("WITH")?;
      Op::StartsWith
    } else if self.keyword("ENDS") {
      self.expect_keyword("WITH")?;
      Op::EndsWith
    } else {
      return Ok(left);
    };
    Ok(Expr::Compare(Box::new(left), op, Box::new(self.operand()?)))
  }

  fn operand(&mut self) -> Result<Expr, QueryError> {
    match self.next() {
      Some(Token::Symbol("(")) => {
        let inner = self.or()?;
        self.expect_symbol(")")?;
        Ok(inner)
      }
      Some(Token::Text(text)) => Ok(Expr::Literal(Cell::Text(text))),
      Some(Token::Int(value)) => Ok(Expr::Literal(Cell::Int(value))),
      Some(Token::Word(word)) if word.eq_ignore_ascii_case("true") => {
        Ok(Expr::Literal(Cell::Bool(true)))
      }
      Some(Token::Word(word)) if word.eq_ignore_ascii_case("false") => {
        Ok(Expr::Literal(Cell::Bool(false)))
      }
      Some(Token::Word(word)) => Ok(Expr::Column(word.to_ascii_lowercase())),
      other => Err(QueryError::Syntax(format!(
        "expected a value, found {other:?}"
      ))),
    }
  }
}

/// Parses `text` as a query.
fn parse(text: &str) -> Result<Query, QueryError> {
  let mut parser = Parser {
    tokens: lex(text)?,
    at: 0,
  };
  parser.query()
}

// ------------------------------------------------------------------------------------------------- execution

/// The columns each source offers, in order (`content` last, and never selected by default).
fn columns_of(from: &From) -> &'static [&'static str] {
  match from {
    From::Volumes => &["id", "name", "referenced", "unique"],
    From::Files { .. } => &["path", "name", "ext", "dir", "kind", "size", "content"],
    From::Lines { .. } => &["path", "line", "text"],
    From::Changed { .. } => &["path"],
  }
}

/// A query's ceilings ([`Ceilings::DERIVED`] in service; a test narrows them to reach each).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ceilings {
  /// Entries a walk may visit.
  pub visits: u64,
  /// File bytes a query may read.
  pub read_bytes: u64,
  /// Bytes the encoded answer may take.
  pub output_bytes: usize,
}

impl Ceilings {
  /// The ceilings a served query runs under: [`MAX_VISITS`], [`MAX_READ_BYTES`], [`MAX_OUTPUT_BYTES`].
  pub const DERIVED: Ceilings = Ceilings {
    visits: MAX_VISITS,
    read_bytes: MAX_READ_BYTES,
    output_bytes: MAX_OUTPUT_BYTES,
  };
}

/// The work a query has done, against its ceilings.
struct Work {
  ceilings: Ceilings,
  visited: u64,
  bytes_read: u64,
}

impl Work {
  fn visit(&mut self) -> Result<(), QueryError> {
    self.visited = self.visited.saturating_add(1);
    if self.visited > self.ceilings.visits {
      return Err(QueryError::Ceiling {
        name: "visits",
        limit: self.ceilings.visits,
      });
    }
    Ok(())
  }

  fn read(&mut self, bytes: usize) -> Result<(), QueryError> {
    self.bytes_read = self
      .bytes_read
      .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    if self.bytes_read > self.ceilings.read_bytes {
      return Err(QueryError::Ceiling {
        name: "bytes_read",
        limit: self.ceilings.read_bytes,
      });
    }
    Ok(())
  }
}

/// One candidate row: its known columns, and how to read its content if a column asks for it.
struct Candidate {
  cells: BTreeMap<&'static str, Cell>,
  content: Option<(VolumeId, ReadAt, String)>,
}

/// Runs `text` against `source` under the derived ceilings.
pub fn run(source: &mut dyn Source, text: &str) -> Result<Answer, QueryError> {
  run_within(source, text, Ceilings::DERIVED)
}

/// Runs `text` against `source` under `ceilings`.
pub fn run_within(
  source: &mut dyn Source,
  text: &str,
  ceilings: Ceilings,
) -> Result<Answer, QueryError> {
  let query = parse(text)?;
  let available = columns_of(&query.from);
  let columns: Vec<String> = match &query.select {
    Some(selected) => selected.clone(),
    None => available
      .iter()
      .filter(|column| **column != "content")
      .map(|column| (*column).to_owned())
      .collect(),
  };
  for column in columns
    .iter()
    .chain(query.order.iter().map(|(column, _)| column))
  {
    if !available.contains(&column.as_str()) {
      return Err(QueryError::Unknown(format!("column {column}")));
    }
  }
  if let Some(filter) = &query.filter {
    check_columns(filter, available)?;
  }
  let early_stop = if query.order.is_empty() {
    query.limit
  } else {
    None
  };
  let mut work = Work {
    ceilings,
    visited: 0,
    bytes_read: 0,
  };
  let mut matched_rows: Vec<Vec<Cell>> = Vec::new();
  let mut matched = 0u64;
  let mut emit = |candidate: &mut Candidate,
                  work: &mut Work,
                  source: &mut dyn Source|
   -> Result<bool, QueryError> {
    if let Some(filter) = &query.filter
      && !eval(filter, candidate, work, source)?.truthy()
    {
      return Ok(true);
    }
    matched = matched.saturating_add(1);
    let mut row = Vec::with_capacity(columns.len());
    for column in &columns {
      row.push(cell(column, candidate, work, source)?);
    }
    matched_rows.push(row);
    Ok(early_stop.is_none_or(|limit| matched < limit))
  };
  produce(&query.from, source, &mut work, &mut emit)?;
  let mut rows = matched_rows;
  if !query.order.is_empty() {
    let positions: Vec<(usize, bool)> = query
      .order
      .iter()
      .filter_map(|(column, descending)| {
        columns
          .iter()
          .position(|c| c == column)
          .map(|at| (at, *descending))
      })
      .collect();
    if positions.len() != query.order.len() {
      return Err(QueryError::Unknown(
        "ORDER BY a column that is not selected".to_owned(),
      ));
    }
    rows.sort_by(|a, b| {
      for (at, descending) in &positions {
        let ordering = a.get(*at).cmp(&b.get(*at));
        let ordering = if *descending {
          ordering.reverse()
        } else {
          ordering
        };
        if ordering != std::cmp::Ordering::Equal {
          return ordering;
        }
      }
      std::cmp::Ordering::Equal
    });
  }
  if let Some(limit) = query.limit {
    rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
  }
  let answer = Answer {
    columns,
    rows,
    matched,
    visited: work.visited,
    bytes_read: work.bytes_read,
  };
  // The answer as the client receives it: its rows encoded as JSON.
  let encoded = serde_json::to_vec(
    &answer
      .rows
      .iter()
      .map(|row| serde_json::Value::Array(row.iter().map(Cell::to_json).collect()))
      .collect::<Vec<_>>(),
  )
  .map_or(usize::MAX, |bytes| bytes.len());
  if encoded > ceilings.output_bytes {
    return Err(QueryError::Ceiling {
      name: "output_bytes",
      limit: u64::try_from(ceilings.output_bytes).unwrap_or(u64::MAX),
    });
  }
  Ok(answer)
}

/// Every column `expr` names must be one of `available`.
fn check_columns(expr: &Expr, available: &[&str]) -> Result<(), QueryError> {
  match expr {
    Expr::Column(column) if !available.contains(&column.as_str()) => {
      Err(QueryError::Unknown(format!("column {column}")))
    }
    Expr::Column(_) | Expr::Literal(_) => Ok(()),
    Expr::Not(inner) => check_columns(inner, available),
    Expr::And(a, b) | Expr::Or(a, b) | Expr::Compare(a, _, b) => {
      check_columns(a, available)?;
      check_columns(b, available)
    }
  }
}

/// A column's value for `candidate`, reading its content when the column is `content`.
fn cell(
  column: &str,
  candidate: &mut Candidate,
  work: &mut Work,
  source: &mut dyn Source,
) -> Result<Cell, QueryError> {
  if column == "content" && !candidate.cells.contains_key("content") {
    let text = match &candidate.content {
      Some((volume, at, path)) => {
        let bytes = source
          .read(*volume, path, *at)
          .map_err(QueryError::Refused)?;
        work.read(bytes.len())?;
        Cell::Text(String::from_utf8_lossy(&bytes).into_owned())
      }
      None => Cell::Null,
    };
    candidate.cells.insert("content", text);
  }
  Ok(candidate.cells.get(column).cloned().unwrap_or(Cell::Null))
}

/// Evaluates `expr` for `candidate`.
fn eval(
  expr: &Expr,
  candidate: &mut Candidate,
  work: &mut Work,
  source: &mut dyn Source,
) -> Result<Cell, QueryError> {
  Ok(match expr {
    Expr::Column(column) => cell(column, candidate, work, source)?,
    Expr::Literal(value) => value.clone(),
    Expr::Not(inner) => Cell::Bool(!eval(inner, candidate, work, source)?.truthy()),
    Expr::And(a, b) => Cell::Bool(
      eval(a, candidate, work, source)?.truthy() && eval(b, candidate, work, source)?.truthy(),
    ),
    Expr::Or(a, b) => Cell::Bool(
      eval(a, candidate, work, source)?.truthy() || eval(b, candidate, work, source)?.truthy(),
    ),
    Expr::Compare(a, op, b) => {
      let (left, right) = (
        eval(a, candidate, work, source)?,
        eval(b, candidate, work, source)?,
      );
      Cell::Bool(compare(&left, *op, &right))
    }
  })
}

/// Whether `left op right` holds.
fn compare(left: &Cell, op: Op, right: &Cell) -> bool {
  match op {
    Op::Eq => left == right,
    Op::Ne => left != right,
    Op::Lt => left < right,
    Op::Le => left <= right,
    Op::Gt => left > right,
    Op::Ge => left >= right,
    Op::Contains => left.text().contains(&right.text()),
    Op::StartsWith => left.text().starts_with(&right.text()),
    Op::EndsWith => left.text().ends_with(&right.text()),
    Op::Glob => glob(&right.text(), &left.text()),
  }
}

/// Whether `path` matches `pattern`: `**` spans any components (none included), `*` any characters within one.
fn glob(pattern: &str, path: &str) -> bool {
  let pattern: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
  let path: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
  glob_parts(&pattern, &path)
}

fn glob_parts(pattern: &[&str], path: &[&str]) -> bool {
  match pattern.split_first() {
    None => path.is_empty(),
    Some((&"**", rest)) => {
      (0..=path.len()).any(|skip| glob_parts(rest, path.get(skip..).unwrap_or_default()))
    }
    Some((first, rest)) => match path.split_first() {
      Some((component, others)) => glob_component(first, component) && glob_parts(rest, others),
      None => false,
    },
  }
}

fn glob_component(pattern: &str, text: &str) -> bool {
  match pattern.split_once('*') {
    None => pattern == text,
    Some((head, tail)) => {
      let Some(rest) = text.strip_prefix(head) else {
        return false;
      };
      (0..=rest.len()).any(|skip| {
        rest.is_char_boundary(skip) && glob_component(tail, rest.get(skip..).unwrap_or_default())
      })
    }
  }
}

/// A volume named by its hex id, or by its name among the volumes.
fn resolve_volume(source: &mut dyn Source, named: &str) -> Result<VolumeId, QueryError> {
  if let Some(id) = crate::id_from_hex(named) {
    return Ok(id);
  }
  source
    .volumes()
    .map_err(QueryError::Refused)?
    .into_iter()
    .find(|(_, name, _, _)| name == named)
    .map(|(id, _, _, _)| id)
    .ok_or_else(|| QueryError::Unknown(format!("volume {named}")))
}

type Emit<'a> =
  dyn FnMut(&mut Candidate, &mut Work, &mut dyn Source) -> Result<bool, QueryError> + 'a;

/// Produces the source's candidates into `emit` until it is exhausted or `emit` answers `false`.
fn produce(
  from: &From,
  source: &mut dyn Source,
  work: &mut Work,
  emit: &mut Emit<'_>,
) -> Result<(), QueryError> {
  match from {
    From::Volumes => {
      for (id, name, referenced, unique) in source.volumes().map_err(QueryError::Refused)? {
        work.visit()?;
        let mut candidate = Candidate {
          cells: BTreeMap::from([
            ("id", Cell::Text(crate::id_hex(id))),
            ("name", Cell::Text(name)),
            ("referenced", Cell::Int(referenced)),
            ("unique", Cell::Int(unique)),
          ]),
          content: None,
        };
        if !emit(&mut candidate, work, source)? {
          return Ok(());
        }
      }
      Ok(())
    }
    From::Changed { volume, since } => {
      let green = resolve_volume(source, volume)?;
      for path in source.changed(green, *since).map_err(QueryError::Refused)? {
        work.visit()?;
        let mut candidate = Candidate {
          cells: BTreeMap::from([("path", Cell::Text(path))]),
          content: None,
        };
        if !emit(&mut candidate, work, source)? {
          return Ok(());
        }
      }
      Ok(())
    }
    From::Files {
      volume,
      version,
      under,
    }
    | From::Lines {
      volume,
      version,
      under,
    } => {
      let id = resolve_volume(source, volume)?;
      let at = version.map_or(ReadAt::Head, |version| ReadAt::Version { version });
      let lines = matches!(from, From::Lines { .. });
      walk(source, (id, at, lines), under, work, emit).map(|_| ())
    }
  }
}

/// Walks `dir` depth first, entries in listing order: `false` once `emit` has asked to stop.
fn walk(
  source: &mut dyn Source,
  (volume, at, lines): (VolumeId, ReadAt, bool),
  dir: &str,
  work: &mut Work,
  emit: &mut Emit<'_>,
) -> Result<bool, QueryError> {
  let entries = source.list(volume, dir, at).map_err(QueryError::Refused)?;
  for (name, kind, size) in entries {
    work.visit()?;
    let path = if dir.is_empty() {
      name.clone()
    } else {
      format!("{dir}/{name}")
    };
    if lines {
      if kind == EntryKind::File && !emit_lines(source, (volume, at), &path, work, emit)? {
        return Ok(false);
      }
    } else {
      let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_owned())
        .unwrap_or_default();
      let mut candidate = Candidate {
        cells: BTreeMap::from([
          ("path", Cell::Text(path.clone())),
          ("name", Cell::Text(name.clone())),
          ("ext", Cell::Text(ext)),
          ("dir", Cell::Text(dir.to_owned())),
          ("kind", Cell::Text(kind_name(kind).to_owned())),
          ("size", Cell::Int(size)),
        ]),
        content: (kind == EntryKind::File).then(|| (volume, at, path.clone())),
      };
      if !emit(&mut candidate, work, source)? {
        return Ok(false);
      }
    }
    if kind == EntryKind::Dir && !walk(source, (volume, at, lines), &path, work, emit)? {
      return Ok(false);
    }
  }
  Ok(true)
}

/// Emits each line of the file at `path` (numbered from 1): `false` once `emit` has asked to stop.
fn emit_lines(
  source: &mut dyn Source,
  (volume, at): (VolumeId, ReadAt),
  path: &str,
  work: &mut Work,
  emit: &mut Emit<'_>,
) -> Result<bool, QueryError> {
  let bytes = source.read(volume, path, at).map_err(QueryError::Refused)?;
  work.read(bytes.len())?;
  let text = String::from_utf8_lossy(&bytes).into_owned();
  for (number, line) in text.lines().enumerate() {
    let mut candidate = Candidate {
      cells: BTreeMap::from([
        ("path", Cell::Text(path.to_owned())),
        (
          "line",
          Cell::Int(u64::try_from(number).unwrap_or(u64::MAX).saturating_add(1)),
        ),
        ("text", Cell::Text(line.to_owned())),
      ]),
      content: None,
    };
    if !emit(&mut candidate, work, source)? {
      return Ok(false);
    }
  }
  Ok(true)
}

/// The query language's name for an entry kind.
fn kind_name(kind: EntryKind) -> &'static str {
  match kind {
    EntryKind::File => "file",
    EntryKind::Dir => "dir",
    EntryKind::Symlink => "symlink",
    EntryKind::Other => "other",
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
  use super::*;

  /// An in-memory volume: its files by path.
  struct Tree {
    files: BTreeMap<String, Vec<u8>>,
    reads: u64,
  }

  fn tree() -> Tree {
    let files = [
      ("README.md", "slates\n"),
      ("src/lib.rs", "pub fn a() {}\nunsafe fn b() {}\n"),
      ("src/deep/x.rs", "fn x() {}\n"),
      ("src/big.rs", "unsafe {}\nunsafe {}\nunsafe {}\n"),
      ("docs/notes.txt", "unsafe? no\n"),
    ];
    Tree {
      files: files
        .iter()
        .map(|(path, text)| ((*path).to_owned(), text.as_bytes().to_vec()))
        .collect(),
      reads: 0,
    }
  }

  fn id() -> VolumeId {
    VolumeId { bytes: [9; 16] }
  }

  impl Source for Tree {
    fn volumes(&mut self) -> Result<Vec<(VolumeId, String, u64, u64)>, String> {
      Ok(vec![(id(), "proj".to_owned(), 100, 40)])
    }

    fn list(
      &mut self,
      _: VolumeId,
      path: &str,
      _: ReadAt,
    ) -> Result<Vec<(String, EntryKind, u64)>, String> {
      let prefix = if path.is_empty() {
        String::new()
      } else {
        format!("{path}/")
      };
      let mut entries: BTreeMap<String, (EntryKind, u64)> = BTreeMap::new();
      for (file, bytes) in &self.files {
        if let Some(rest) = file.strip_prefix(&prefix) {
          match rest.split_once('/') {
            Some((dir, _)) => entries.insert(dir.to_owned(), (EntryKind::Dir, 0)),
            None => entries.insert(rest.to_owned(), (EntryKind::File, bytes.len() as u64)),
          };
        }
      }
      Ok(
        entries
          .into_iter()
          .map(|(name, (kind, size))| (name, kind, size))
          .collect(),
      )
    }

    fn read(&mut self, _: VolumeId, path: &str, _: ReadAt) -> Result<Vec<u8>, String> {
      self.reads += 1;
      self
        .files
        .get(path)
        .cloned()
        .ok_or_else(|| format!("no {path}"))
    }

    fn changed(&mut self, _: VolumeId, since: u64) -> Result<Vec<String>, String> {
      Ok(if since == 0 {
        vec!["src/lib.rs".to_owned()]
      } else {
        Vec::new()
      })
    }
  }

  fn rows(answer: &Answer) -> Vec<Vec<String>> {
    answer
      .rows
      .iter()
      .map(|row| row.iter().map(Cell::text).collect())
      .collect()
  }

  /// §4.12 codemode: do query the Rust files containing `unsafe`, by size, the largest first; expect exactly those
  /// files, in that order, with only the selected columns, and the volume named by its name.
  #[test]
  fn a_query_filters_selects_and_orders_files() {
    let mut source = tree();
    let answer = run(
      &mut source,
      r#"FROM files("proj") WHERE ext = "rs" AND content CONTAINS "unsafe" SELECT path, size ORDER BY size DESC"#,
    )
    .unwrap();
    assert_eq!(answer.columns, vec!["path", "size"]);
    assert_eq!(
      rows(&answer),
      vec![
        vec!["src/lib.rs".to_owned(), "31".to_owned()],
        vec!["src/big.rs".to_owned(), "30".to_owned()]
      ]
    );
  }

  /// §4.12 codemode: do find lines containing `unsafe` under `src` with `LIMIT 1` and no ordering; expect one row,
  /// and the walk to have stopped there (fewer files read than a full scan reads) — the early stop is real work
  /// saved, not a truncation after the fact.
  #[test]
  fn a_limit_without_ordering_stops_the_walk_early() {
    let mut full = tree();
    let all = run(
      &mut full,
      r#"FROM lines("proj", under = "src") WHERE text CONTAINS "unsafe""#,
    )
    .unwrap();
    assert_eq!(all.rows.len(), 4);
    let mut limited = tree();
    let one = run(
      &mut limited,
      r#"FROM lines("proj", under = "src") WHERE text CONTAINS "unsafe" LIMIT 1"#,
    )
    .unwrap();
    assert_eq!(one.rows.len(), 1);
    assert!(
      limited.reads < full.reads,
      "stopped early: {} of {} reads",
      limited.reads,
      full.reads
    );
  }

  /// §4.12 codemode: do match paths by glob; expect `**` to span any directories (none included) and `*` to stay in
  /// one component.
  #[test]
  fn globs_span_directories_only_with_two_stars() {
    assert!(glob("src/**/*.rs", "src/lib.rs"));
    assert!(glob("src/**/*.rs", "src/deep/x.rs"));
    assert!(!glob("src/*.rs", "src/deep/x.rs"));
    assert!(glob("**", "a/b/c"));
    assert!(!glob("*.md", "docs/a.md"));
    let mut source = tree();
    let answer = run(
      &mut source,
      r#"FROM files("proj") WHERE path GLOB "src/**/*.rs" SELECT path"#,
    )
    .unwrap();
    assert_eq!(rows(&answer).len(), 3);
  }

  /// §4.12 codemode: do run queries that are not this language or name an unknown column, source or volume; expect
  /// each refused with its kind.
  #[test]
  fn malformed_queries_are_refused_by_kind() {
    let mut source = tree();
    assert!(matches!(
      run(&mut source, "FROM"),
      Err(QueryError::Syntax(_))
    ));
    assert!(matches!(
      run(&mut source, r#"FROM files("proj") SELECT nope"#),
      Err(QueryError::Unknown(_))
    ));
    assert!(matches!(
      run(&mut source, r#"FROM tables("proj")"#),
      Err(QueryError::Unknown(_))
    ));
    assert!(matches!(
      run(&mut source, r#"FROM files("missing")"#),
      Err(QueryError::Unknown(_))
    ));
  }

  /// §4.12 codemode: do run a query past each ceiling; expect each refused by its name and limit, never a partial
  /// answer.
  #[test]
  fn each_ceiling_is_refused_by_name() {
    let mut source = tree();
    let narrow = Ceilings {
      visits: 2,
      read_bytes: 4,
      output_bytes: 8,
    };
    assert_eq!(
      run_within(&mut source, r#"FROM files("proj")"#, narrow).unwrap_err(),
      QueryError::Ceiling {
        name: "visits",
        limit: 2
      }
    );
    assert_eq!(
      run_within(
        &mut source,
        r#"FROM lines("proj", under = "src")"#,
        Ceilings {
          visits: 99,
          ..narrow
        }
      )
      .unwrap_err(),
      QueryError::Ceiling {
        name: "bytes_read",
        limit: 4
      }
    );
    assert_eq!(
      run_within(
        &mut source,
        r#"FROM volumes()"#,
        Ceilings {
          visits: 99,
          read_bytes: 99,
          output_bytes: 8
        }
      )
      .unwrap_err(),
      QueryError::Ceiling {
        name: "output_bytes",
        limit: 8
      }
    );
  }

  /// §4.12 codemode: do list volumes and a green's changes; expect each source's rows.
  #[test]
  fn volumes_and_changes_are_sources() {
    let mut source = tree();
    let volumes = run(&mut source, "FROM volumes() SELECT name, unique").unwrap();
    assert_eq!(
      rows(&volumes),
      vec![vec!["proj".to_owned(), "40".to_owned()]]
    );
    let changed = run(&mut source, r#"FROM changed("proj", since = 0)"#).unwrap();
    assert_eq!(rows(&changed), vec![vec!["src/lib.rs".to_owned()]]);
  }
}
