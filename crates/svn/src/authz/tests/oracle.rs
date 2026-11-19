//! The naive oracle (A-606): a generated rule set (groups that nest without cycles, aliases, plain and glob sections
//! for no repository or one of two, every kind of entry, inverted or not), written out as an authz file in assorted
//! styles and parsed by the module, against an evaluation that takes every generated rule on its own and never builds
//! a tree: a rule matches a level when its segments match that many of the path's, the deepest matching level decides
//! and the last rule defined there wins; a recursive question takes the least access of the path's and of every rule
//! not eclipsed by a later `**` above it that some extension of the path could meet; a question without a path is
//! answered by Subversion's accumulation, transcribed over the generated set. Every principal, four repositories and
//! every path of up to three segments over five names are asked, after every generated set.

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::super::{Access, Authz, Bounds, GlobalRights, Principal, RepositoryPath};

/// The users a generated set may name; queries also ask `u4`, which no set names, and the anonymous user.
const USERS: [&str; 4] = ["u0", "u1", "u2", "u3"];

/// The names path segments are made of.
const NAMES: [&str; 5] = ["a", "b", "ab", "ba", "*"];

/// The repositories asked about: none, the two sections may name, and one no section names.
const REPOSITORIES: [&str; 4] = ["", "R1", "R2", "R3"];

/// A segment of a generated rule's path, as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Piece {
  Name(&'static str),
  One,
  Many,
  Prefix,
  Suffix,
  Pattern(&'static str),
}

impl Piece {
  /// How the piece is written in a section header.
  fn text(self) -> &'static str {
    match self {
      Self::Name(name) | Self::Pattern(name) => name,
      Self::One => "*",
      Self::Many => "**",
      Self::Prefix => "a*",
      Self::Suffix => "*b",
    }
  }
}

/// A segment as the oracle matches it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seg {
  Literal(String),
  One,
  Many,
  Prefix(String),
  Suffix(String),
  Pattern(String),
}

/// Whom a generated entry names.
#[derive(Clone, Copy, Debug)]
enum Who {
  User(usize),
  Group(usize),
  Alias(usize),
  Everyone,
  Anonymous,
  Authenticated,
}

/// A generated access entry.
#[derive(Clone, Copy, Debug)]
struct Entry {
  inverted: bool,
  who: Who,
  access: Access,
}

/// A generated group member.
#[derive(Clone, Copy, Debug)]
enum Member {
  User(usize),
  Alias(usize),
  Group(usize),
}

/// A generated rule section.
#[derive(Clone, Debug)]
struct Section {
  repository: &'static str,
  glob: bool,
  pieces: Vec<Piece>,
  entries: Vec<Entry>,
}

impl Section {
  /// The segments the oracle matches: a glob section's pieces classified and their runs of `*` and `**` normalized; a
  /// plain section's pieces all literal.
  fn segments(&self) -> Vec<Seg> {
    if !self.glob {
      return self
        .pieces
        .iter()
        .map(|piece| Seg::Literal(piece.text().to_owned()))
        .collect();
    }
    let classified: Vec<Seg> = self
      .pieces
      .iter()
      .map(|piece| match piece {
        Piece::Name(name) => Seg::Literal((*name).to_owned()),
        Piece::One => Seg::One,
        Piece::Many => Seg::Many,
        Piece::Prefix => Seg::Prefix("a".to_owned()),
        Piece::Suffix => Seg::Suffix("b".to_owned()),
        Piece::Pattern(pattern) => Seg::Pattern((*pattern).to_owned()),
      })
      .collect();
    normalize(&classified)
  }

  /// The section's header.
  fn header(&self) -> String {
    let glob = if self.glob { ":glob:" } else { "" };
    let repository = if self.repository.is_empty() {
      String::new()
    } else {
      format!("{}:", self.repository)
    };
    let path: Vec<&str> = self.pieces.iter().map(|piece| piece.text()).collect();
    format!("[{glob}{repository}/{}]", path.join("/"))
  }
}

/// Each run of `*` and `**` as its `*`s followed by one `**` if the run held any: the rule the release notes give.
fn normalize(segments: &[Seg]) -> Vec<Seg> {
  let mut out = Vec::new();
  let mut ones = 0usize;
  let mut many = false;
  for segment in segments {
    match segment {
      Seg::One => ones += 1,
      Seg::Many => many = true,
      other => {
        out.extend(std::iter::repeat_n(Seg::One, ones));
        if many {
          out.push(Seg::Many);
        }
        ones = 0;
        many = false;
        out.push(other.clone());
      }
    }
  }
  out.extend(std::iter::repeat_n(Seg::One, ones));
  if many {
    out.push(Seg::Many);
  }
  out
}

/// How a generated set is written out.
#[derive(Clone, Copy, Debug)]
struct Style {
  separator: &'static str,
  crlf: bool,
  byte_order_mark: bool,
  continuation: bool,
  comments: bool,
}

/// A generated rule set.
#[derive(Clone, Debug)]
struct Model {
  /// Alias `a{index}` stands for user `USERS[user]`.
  aliases: Vec<usize>,
  /// Group `g{index}`'s members; a group names only groups before it.
  groups: Vec<Vec<Member>>,
  sections: Vec<Section>,
  groups_file: bool,
  style: Style,
}

impl Model {
  /// Whether `user` belongs to group `group`, by its members, recursively.
  fn in_group(&self, group: usize, user: &str) -> bool {
    self.groups[group].iter().any(|member| match member {
      Member::User(index) => USERS[*index] == user,
      Member::Alias(alias) => USERS[self.aliases[*alias]] == user,
      Member::Group(nested) => self.in_group(*nested, user),
    })
  }

  /// Whether group `group` has no user at all, through its groups either.
  fn group_is_empty(&self, group: usize) -> bool {
    self.groups[group].iter().all(|member| match member {
      Member::User(_) | Member::Alias(_) => false,
      Member::Group(nested) => self.group_is_empty(*nested),
    })
  }

  /// Whether the set has a section for the root of every repository.
  fn has_global_root(&self) -> bool {
    self
      .sections
      .iter()
      .any(|section| section.repository.is_empty() && section.segments().is_empty())
  }

  /// Whether `entry` speaks for `principal`.
  fn entry_applies(&self, entry: &Entry, principal: Option<&str>) -> bool {
    match (principal, entry.who) {
      (_, Who::Everyone) => true,
      (None, Who::Anonymous) | (Some(_), Who::Authenticated) => !entry.inverted,
      (None, Who::Authenticated) | (Some(_), Who::Anonymous) => entry.inverted,
      (None, _) => false,
      (Some(user), Who::User(index)) => (USERS[index] == user) != entry.inverted,
      (Some(user), Who::Alias(alias)) => (USERS[self.aliases[alias]] == user) != entry.inverted,
      (Some(user), Who::Group(group)) => {
        !self.group_is_empty(group) && (self.in_group(group, user) != entry.inverted)
      }
    }
  }

  /// What section `index` gives `principal`, if it speaks for it; the global root speaks for everybody.
  fn section_access(&self, index: usize, principal: Option<&str>) -> Option<Access> {
    let section = &self.sections[index];
    let held = section
      .entries
      .iter()
      .filter(|entry| self.entry_applies(entry, principal))
      .map(|entry| entry.access)
      .max();
    let global_root = section.repository.is_empty() && section.segments().is_empty();
    if global_root {
      Some(held.unwrap_or(Access::None))
    } else {
      held
    }
  }

  /// The rules that apply to `principal` in `repository`, one per path: the repository's section if it speaks for the
  /// principal, else the global one; the root's default when the set has no global root.
  fn kept(&self, repository: &str, principal: Option<&str>) -> Vec<Rule> {
    let mut paths: Vec<(Vec<Seg>, Option<Rule>, Option<Rule>)> = Vec::new();
    let mut offer = |segments: Vec<Seg>, global: bool, rule: Rule| {
      let slot = match paths.iter_mut().position(|(held, _, _)| *held == segments) {
        Some(at) => at,
        None => {
          paths.push((segments, None, None));
          paths.len() - 1
        }
      };
      if global {
        paths[slot].1 = Some(rule);
      } else {
        paths[slot].2 = Some(rule);
      }
    };
    for (index, section) in self.sections.iter().enumerate() {
      let global = section.repository.is_empty();
      if !global && section.repository != repository {
        continue;
      }
      if let Some(access) = self.section_access(index, principal) {
        let segments = section.segments();
        offer(
          segments.clone(),
          global,
          Rule {
            sequence: index + 1,
            segments,
            access,
          },
        );
      }
    }
    if !self.has_global_root() {
      offer(
        Vec::new(),
        true,
        Rule {
          sequence: 0,
          segments: Vec::new(),
          access: Access::None,
        },
      );
    }
    paths
      .into_iter()
      .filter_map(|(_, global, own)| own.or(global))
      .collect()
  }

  /// The rules file and the groups file, written in the set's style.
  fn render(&self) -> (String, Option<String>) {
    let line = if self.style.crlf { "\r\n" } else { "\n" };
    let separator = self.style.separator;
    let mut rules = String::new();
    if self.style.byte_order_mark {
      rules.push('\u{feff}');
    }
    if !self.aliases.is_empty() {
      rules.push_str(&format!("[aliases]{line}"));
      for (index, user) in self.aliases.iter().enumerate() {
        rules.push_str(&format!("a{index}{separator}{}{line}", USERS[*user]));
      }
    }
    let groups = self.render_groups(line);
    if !self.groups_file {
      rules.push_str(&groups);
    }
    for section in &self.sections {
      if self.style.comments {
        rules.push_str(&format!("# {}{line}", section.header()));
      }
      rules.push_str(&format!("{}{line}", section.header()));
      for entry in &section.entries {
        rules.push_str(&format!(
          "{}{separator}{}{line}",
          entry_name(entry),
          self.value(entry.access, line)
        ));
      }
    }
    (rules, self.groups_file.then_some(groups))
  }

  /// The `[groups]` section.
  fn render_groups(&self, line: &str) -> String {
    let mut text = format!("[groups]{line}");
    for (index, members) in self.groups.iter().enumerate() {
      let names: Vec<String> = members
        .iter()
        .map(|member| match member {
          Member::User(user) => USERS[*user].to_owned(),
          Member::Alias(alias) => format!("&a{alias}"),
          Member::Group(group) => format!("@g{group}"),
        })
        .collect();
      let joiner = if self.style.continuation {
        format!(",{line}  ")
      } else {
        ", ".to_owned()
      };
      text.push_str(&format!(
        "g{index}{}{}{line}",
        self.style.separator,
        names.join(&joiner)
      ));
    }
    text
  }

  /// An access value; read and write written over two lines when the style continues values.
  fn value(&self, access: Access, line: &str) -> String {
    match access {
      Access::None => String::new(),
      Access::Read => "r".to_owned(),
      Access::Write if self.style.continuation => format!("r{line}  w"),
      Access::Write => "rw".to_owned(),
    }
  }

  /// The users the set names anywhere: in entries, as group members, as aliases' users.
  fn known(&self, user: &str) -> bool {
    let in_entries = self
      .sections
      .iter()
      .flat_map(|section| &section.entries)
      .any(|entry| matches!(entry.who, Who::User(index) if USERS[index] == user));
    let in_groups = self
      .groups
      .iter()
      .flatten()
      .any(|member| matches!(member, Member::User(index) if USERS[*index] == user));
    let aliased = self.aliases.iter().any(|index| USERS[*index] == user);
    in_entries || in_groups || aliased
  }
}

/// An entry's name as written.
fn entry_name(entry: &Entry) -> String {
  let inversion = if entry.inverted { "~" } else { "" };
  let name = match entry.who {
    Who::User(index) => USERS[index].to_owned(),
    Who::Group(group) => format!("@g{group}"),
    Who::Alias(alias) => format!("&a{alias}"),
    Who::Everyone => "*".to_owned(),
    Who::Anonymous => "$anonymous".to_owned(),
    Who::Authenticated => "$authenticated".to_owned(),
  };
  format!("{inversion}{name}")
}

/// A rule as the oracle weighs it.
#[derive(Clone, Debug)]
struct Rule {
  sequence: usize,
  segments: Vec<Seg>,
  access: Access,
}

/// Whether `pattern` matches the whole of `name`: `*` any run, `?` one byte, anything else itself.
fn naive_fnmatch(pattern: &[u8], name: &[u8]) -> bool {
  match pattern.split_first() {
    None => name.is_empty(),
    Some((b'*', rest)) => (0..=name.len()).any(|skip| naive_fnmatch(rest, &name[skip..])),
    Some((b'?', rest)) => !name.is_empty() && naive_fnmatch(rest, &name[1..]),
    Some((byte, rest)) => name.first() == Some(byte) && naive_fnmatch(rest, &name[1..]),
  }
}

/// Whether `segments` match exactly the names `path`, `**` taking any number of them.
fn full_match(segments: &[Seg], path: &[&str]) -> bool {
  match segments.split_first() {
    None => path.is_empty(),
    Some((Seg::Many, rest)) => (0..=path.len()).any(|skip| full_match(rest, &path[skip..])),
    Some((segment, rest)) => match path.split_first() {
      None => false,
      Some((name, others)) => {
        let matched = match segment {
          Seg::Literal(literal) => literal == name,
          Seg::One | Seg::Many => true,
          Seg::Prefix(prefix) => name.starts_with(prefix.as_str()),
          Seg::Suffix(suffix) => name.ends_with(suffix.as_str()),
          Seg::Pattern(pattern) => naive_fnmatch(pattern.as_bytes(), name.as_bytes()),
        };
        matched && full_match(rest, others)
      }
    },
  }
}

/// The access at `path`: the deepest level some rule matches, then the rule defined last there.
fn oracle_access(rules: &[Rule], path: &[&str]) -> Access {
  for level in (0..=path.len()).rev() {
    let deciding = rules
      .iter()
      .filter(|rule| full_match(&rule.segments, &path[..level]))
      .max_by_key(|rule| rule.sequence);
    if let Some(rule) = deciding {
      return rule.access;
    }
  }
  Access::None
}

/// Whether a later rule ending in `**`, at a prefix of `rule`'s path, eclipses `rule`.
fn eclipsed(rule: &Rule, rules: &[Rule]) -> bool {
  (0..=rule.segments.len()).any(|cut| {
    if cut > 0 && rule.segments[cut - 1] == Seg::Many {
      return false;
    }
    let mut eclipsing = rule.segments[..cut].to_vec();
    eclipsing.push(Seg::Many);
    rules
      .iter()
      .any(|other| other.segments == eclipsing && other.sequence > rule.sequence)
  })
}

/// The access at `path` and everywhere beneath it: the least of the path's and of every uneclipsed rule some
/// extension of the path could meet.
fn oracle_beneath(rules: &[Rule], path: &[&str]) -> Access {
  rules
    .iter()
    .filter(|rule| !eclipsed(rule, rules))
    .filter(|rule| (0..=rule.segments.len()).any(|cut| full_match(&rule.segments[..cut], path)))
    .map(|rule| rule.access)
    .fold(oracle_access(rules, path), Access::min)
}

/// Least and most access, accumulated as `authz_rights_t` is.
#[derive(Clone, Copy, Debug)]
struct Rights {
  least: Access,
  most: Access,
}

/// Accumulates `(repository, access)` pairs for the repository `asked` and resolves them as Subversion does.
fn accumulate(pairs: &[(&str, Access)], asked: &str) -> (Rights, bool) {
  let unset = Rights {
    least: Access::Write,
    most: Access::None,
  };
  let fold = |rights: Rights, access: Access| Rights {
    least: rights.least.min(access),
    most: rights.most.max(access),
  };
  let any = pairs
    .iter()
    .filter(|(repository, _)| repository.is_empty())
    .fold(unset, |rights, (_, access)| fold(rights, *access));
  let named: Vec<Access> = pairs
    .iter()
    .filter(|(repository, _)| !repository.is_empty() && *repository == asked)
    .map(|(_, access)| *access)
    .collect();
  if asked.is_empty() {
    return (any, true);
  }
  if named.is_empty() {
    return (any, false);
  }
  let named = named.into_iter().fold(unset, fold);
  (
    Rights {
      least: named.least.min(any.least),
      most: named.most.max(any.most),
    },
    true,
  )
}

/// What one section (or the root's default) gives the anonymous user, every authenticated user, and through its
/// inverted entries, as `add_access_entry` accumulates them.
#[derive(Clone, Copy, Debug)]
struct TokenRow {
  repository: &'static str,
  anonymous: Option<Access>,
  authenticated: Option<Access>,
  inverted: Option<Access>,
}

/// Every section's [`TokenRow`], and the root's default when the set has no global root.
fn token_access(model: &Model) -> Vec<TokenRow> {
  let mut sections: Vec<TokenRow> = model
    .sections
    .iter()
    .map(|section| {
      let pick = |wanted: &dyn Fn(&Entry) -> bool| {
        section
          .entries
          .iter()
          .filter(|entry| wanted(entry))
          .map(|entry| entry.access)
          .max()
      };
      let anonymous = pick(&|entry| {
        matches!(
          (entry.who, entry.inverted),
          (Who::Everyone, _) | (Who::Anonymous, false) | (Who::Authenticated, true)
        )
      });
      let authenticated = pick(&|entry| {
        matches!(
          (entry.who, entry.inverted),
          (Who::Everyone, _) | (Who::Authenticated, false) | (Who::Anonymous, true)
        )
      });
      let inverted = pick(&|entry| {
        entry.inverted && matches!(entry.who, Who::User(_) | Who::Group(_) | Who::Alias(_))
      });
      let global_root = section.repository.is_empty() && section.segments().is_empty();
      let forced = |access: Option<Access>| {
        if global_root {
          Some(access.unwrap_or(Access::None))
        } else {
          access
        }
      };
      TokenRow {
        repository: section.repository,
        anonymous: forced(anonymous),
        authenticated: forced(authenticated),
        inverted,
      }
    })
    .collect();
  if !model.has_global_root() {
    sections.push(TokenRow {
      repository: "",
      anonymous: Some(Access::None),
      authenticated: Some(Access::None),
      inverted: Some(Access::None),
    });
  }
  sections
}

/// `svn_authz__get_global_rights` transcribed over the generated set.
fn oracle_global(model: &Model, repository: &str, principal: Option<&str>) -> GlobalRights {
  let tokens = token_access(model);
  let column = |pick: fn(&TokenRow) -> Option<Access>| {
    tokens
      .iter()
      .filter_map(|row| pick(row).map(|access| (row.repository, access)))
      .collect::<Vec<_>>()
  };
  let (rights, explicit) = match principal {
    None => accumulate(&column(|row| row.anonymous), repository),
    Some(user) => {
      let own = if model.known(user) {
        let mut pairs: Vec<(&str, Access)> = (0..model.sections.len())
          .filter_map(|index| {
            model
              .section_access(index, Some(user))
              .map(|access| (model.sections[index].repository, access))
          })
          .collect();
        if !model.has_global_root() {
          pairs.push(("", Access::None));
        }
        Some(accumulate(&pairs, repository))
      } else if tokens.iter().any(|row| row.inverted.is_some()) {
        Some(accumulate(&column(|row| row.inverted), repository))
      } else {
        None
      };
      let (everyone, everyone_explicit) = accumulate(&column(|row| row.authenticated), repository);
      match own {
        Some((own, own_explicit)) => (
          Rights {
            least: own.least.min(everyone.least),
            most: own.most.max(everyone.most),
          },
          own_explicit || everyone_explicit,
        ),
        None => (everyone, everyone_explicit),
      }
    }
  };
  GlobalRights {
    every_path: rights.least,
    some_path: rights.most,
    explicit,
  }
}

/// Every path of up to three segments over [`NAMES`], the root first.
fn paths() -> Vec<Vec<&'static str>> {
  let mut all = vec![Vec::new()];
  let mut level = vec![Vec::new()];
  for _ in 0..3 {
    level = level
      .iter()
      .flat_map(|path: &Vec<&'static str>| {
        NAMES.iter().map(move |name| {
          let mut longer = path.clone();
          longer.push(*name);
          longer
        })
      })
      .collect();
    all.extend(level.iter().cloned());
  }
  all
}

/// The generated set parsed by the module and asked everything, against the oracle.
fn agrees(model: &Model) -> Result<(), TestCaseError> {
  let (rules, groups) = model.render();
  let groups_bytes = groups.as_deref().map(str::as_bytes);
  let authz = Authz::parse(
    rules.as_bytes(),
    groups_bytes,
    Bounds::of(rules.as_bytes(), groups_bytes),
  )
  .map_err(|refused| TestCaseError::fail(format!("{refused:?} refused:\n{rules}\n{groups:?}")))?;
  let users = [
    None,
    Some("u0"),
    Some("u1"),
    Some("u2"),
    Some("u3"),
    Some("u4"),
  ];
  let every_path = paths();
  for repository in REPOSITORIES {
    for user in users {
      let principal = Principal::named(user.map(str::as_bytes));
      let compiled = authz.rules(repository.as_bytes(), principal);
      let kept = model.kept(repository, user);
      let global = oracle_global(model, repository, user);
      prop_assert_eq!(
        authz.global_rights(repository.as_bytes(), principal),
        global,
        "global rights in {:?} for {:?}:\n{}",
        repository,
        user,
        rules
      );
      prop_assert_eq!(
        compiled.access_anywhere(),
        global.every_path.max(global.some_path)
      );
      for path in &every_path {
        let text = format!("/{}", path.join("/"));
        let at = RepositoryPath::new(text.as_bytes())
          .map_err(|refused| TestCaseError::fail(format!("{refused:?}")))?;
        prop_assert_eq!(
          compiled.access(at),
          oracle_access(&kept, path),
          "{} in {:?} for {:?}:\n{}",
          text,
          repository,
          user,
          rules
        );
        prop_assert_eq!(
          compiled.access_beneath(at),
          oracle_beneath(&kept, path),
          "beneath {} in {:?} for {:?}:\n{}",
          text,
          repository,
          user,
          rules
        );
      }
    }
  }
  Ok(())
}

/// The pieces a rule path is made of.
const PIECES: [Piece; 10] = [
  Piece::Name("a"),
  Piece::Name("b"),
  Piece::Name("ab"),
  Piece::One,
  Piece::Many,
  Piece::Prefix,
  Piece::Suffix,
  Piece::Pattern("?"),
  Piece::Pattern("a?"),
  Piece::Pattern("*a*"),
];

/// The repositories a section may name.
const SECTION_REPOSITORIES: [&str; 3] = ["", "R1", "R2"];

/// The separators an option may be written with.
const SEPARATORS: [&str; 4] = [" = ", "=", ": ", " : "];

/// A generated piece of a rule path, chosen by index: a plain strategy, nothing `Arc`-backed (R2).
fn piece() -> impl Strategy<Value = Piece> {
  (0..PIECES.len()).prop_map(|at| PIECES[at])
}

/// A generated access.
fn access() -> impl Strategy<Value = Access> {
  (0u8..3).prop_map(|at| match at {
    0 => Access::None,
    1 => Access::Read,
    _ => Access::Write,
  })
}

/// Whom a generated entry names, from a plain tuple.
fn who() -> impl Strategy<Value = Who> {
  (0u8..6, 0..USERS.len(), 0..3usize, 0..2usize).prop_map(|(kind, user, group, alias)| match kind {
    0 => Who::User(user),
    1 => Who::Group(group),
    2 => Who::Alias(alias),
    3 => Who::Everyone,
    4 => Who::Anonymous,
    _ => Who::Authenticated,
  })
}

/// A generated entry (`~*` never: it is refused).
fn entry() -> impl Strategy<Value = Entry> {
  (any::<bool>(), who(), access()).prop_map(|(inverted, who, access)| Entry {
    inverted: inverted && !matches!(who, Who::Everyone),
    who,
    access,
  })
}

/// A generated section.
fn section() -> impl Strategy<Value = Section> {
  (
    0..SECTION_REPOSITORIES.len(),
    any::<bool>(),
    vec(piece(), 0..=3),
    vec(entry(), 0..=4),
  )
    .prop_map(|(repository, glob, pieces, entries)| Section {
      repository: SECTION_REPOSITORIES[repository],
      glob,
      pieces,
      entries,
    })
}

/// A generated member, from a plain tuple.
fn member() -> impl Strategy<Value = Member> {
  (0u8..3, 0..USERS.len(), 0..2usize, 0..3usize).prop_map(|(kind, user, alias, group)| match kind {
    0 => Member::User(user),
    1 => Member::Alias(alias),
    _ => Member::Group(group),
  })
}

/// A generated style.
fn style() -> impl Strategy<Value = Style> {
  (
    0..SEPARATORS.len(),
    any::<bool>(),
    any::<bool>(),
    any::<bool>(),
    any::<bool>(),
  )
    .prop_map(
      |(separator, crlf, byte_order_mark, continuation, comments)| Style {
        separator: SEPARATORS[separator],
        crlf,
        byte_order_mark,
        continuation,
        comments,
      },
    )
}

/// A generated rule set, every name it uses defined and every rule unique.
fn model() -> impl Strategy<Value = Model> {
  (
    vec(0..USERS.len(), 0..=2),
    vec(vec(member(), 0..=3), 0..=3),
    vec(section(), 0..=6),
    any::<bool>(),
    style(),
  )
    .prop_map(|(aliases, groups, sections, groups_file, style)| {
      let fix_member = |member: Member, before: usize| match member {
        Member::Alias(alias) if aliases.is_empty() => Member::User(alias % USERS.len()),
        Member::Alias(alias) => Member::Alias(alias % aliases.len()),
        Member::Group(group) if before == 0 => Member::User(group % USERS.len()),
        Member::Group(group) => Member::Group(group % before),
        user => user,
      };
      let groups: Vec<Vec<Member>> = groups
        .into_iter()
        .enumerate()
        .map(|(index, members)| {
          members
            .into_iter()
            .map(|member| fix_member(member, index))
            .collect()
        })
        .collect();
      let fix_who = |who: Who| match who {
        Who::Group(group) if groups.is_empty() => Who::User(group % USERS.len()),
        Who::Group(group) => Who::Group(group % groups.len()),
        Who::Alias(alias) if aliases.is_empty() => Who::User(alias % USERS.len()),
        Who::Alias(alias) => Who::Alias(alias % aliases.len()),
        other => other,
      };
      let mut unique: Vec<Section> = Vec::new();
      for mut section in sections {
        for entry in &mut section.entries {
          entry.who = fix_who(entry.who);
        }
        let repeated = unique.iter().any(|held| {
          held.repository == section.repository
            && (held.segments() == section.segments() || held.header() == section.header())
        });
        if !repeated {
          unique.push(section);
        }
      }
      Model {
        aliases,
        groups,
        sections: unique,
        groups_file,
        style,
      }
    })
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// Every generated rule set, written out and parsed, answers every question as the naive oracle does.
  #[test]
  fn the_module_agrees_with_the_naive_oracle(model in model()) {
    agrees(&model)?;
  }
}

/// A hand-built rule set that uses every construct: a groups file, nested groups, an alias as a member and as an
/// entry, inverted entries, every piece kind, a global and a repository rule for the root, a later `**` over an
/// earlier rule, an entry for an empty group, and every style at once.
fn rich_model() -> Model {
  let entry = |inverted: bool, who: Who, access: Access| Entry {
    inverted,
    who,
    access,
  };
  let section =
    |repository: &'static str, glob: bool, pieces: Vec<Piece>, entries: Vec<Entry>| Section {
      repository,
      glob,
      pieces,
      entries,
    };
  Model {
    aliases: vec![1, 2],
    groups: vec![
      vec![Member::User(0), Member::Alias(0)],
      vec![Member::Group(0), Member::User(3)],
      Vec::new(),
    ],
    sections: vec![
      section(
        "",
        false,
        vec![],
        vec![entry(false, Who::Everyone, Access::Read)],
      ),
      section(
        "R1",
        false,
        vec![],
        vec![entry(false, Who::Group(1), Access::Write)],
      ),
      section(
        "",
        false,
        vec![Piece::Name("a"), Piece::Name("b")],
        vec![entry(false, Who::User(1), Access::Write)],
      ),
      section(
        "",
        true,
        vec![Piece::Name("a"), Piece::Many],
        vec![
          entry(true, Who::Group(0), Access::None),
          entry(false, Who::Alias(1), Access::Write),
        ],
      ),
      section(
        "R2",
        true,
        vec![Piece::Prefix, Piece::Suffix],
        vec![
          entry(true, Who::User(2), Access::Read),
          entry(false, Who::Authenticated, Access::None),
        ],
      ),
      section(
        "",
        true,
        vec![Piece::One, Piece::Pattern("*a*")],
        vec![
          entry(false, Who::Anonymous, Access::Write),
          entry(false, Who::Group(2), Access::Write),
        ],
      ),
    ],
    groups_file: true,
    style: Style {
      separator: " : ",
      crlf: true,
      byte_order_mark: true,
      continuation: true,
      comments: true,
    },
  }
}

/// The oracle's comparison is not vacuous: over the rich set, the module agrees with it, its answers take every
/// access, a recursive answer falls below a path's own, and some principal's rules are walked rather than uniform.
#[test]
fn the_oracle_compares_varied_answers_over_a_rich_rule_set() {
  let model = rich_model();
  agrees(&model).expect("the module agrees with the oracle");
  let mut seen = [false; 3];
  let mut beneath_lower = false;
  for repository in REPOSITORIES {
    for user in [
      None,
      Some("u0"),
      Some("u1"),
      Some("u2"),
      Some("u3"),
      Some("u4"),
    ] {
      let kept = model.kept(repository, user);
      for path in paths() {
        let access = oracle_access(&kept, &path);
        seen[access as usize] = true;
        beneath_lower |= oracle_beneath(&kept, &path) < access;
      }
    }
  }
  assert_eq!(seen, [true; 3], "every access answered");
  assert!(beneath_lower, "a recursive answer below a path's own");
  let (rules, groups) = model.render();
  let authz = Authz::parse(
    rules.as_bytes(),
    groups.as_deref().map(str::as_bytes),
    Bounds::of(rules.as_bytes(), groups.as_deref().map(str::as_bytes)),
  )
  .expect("parses");
  let walked = REPOSITORIES.iter().any(|repository| {
    [None, Some("u0"), Some("u2")].iter().any(|user| {
      authz
        .rules(
          repository.as_bytes(),
          Principal::named(user.map(str::as_bytes)),
        )
        .uniform()
        .is_none()
    })
  });
  assert!(walked, "some principal's rules are walked");
}
