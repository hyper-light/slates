//! The authz file's sections, entries, groups and aliases (A-606): `authz_parse.c`'s constructor callbacks
//! (`rules_open_section`, `rules_add_value`, `groups_open_section`, `groups_add_value`, `add_access_entry`) as a
//! [`Sink`] of the line reader, then the pass Subversion makes once both files are read (`svn_authz__parse`): every
//! group's members resolved, every alias and group an entry names checked, entries for empty groups dropped, and the
//! default rule at the root put in place.
//!
//! Subversion expands each group into its set of users. Nothing here expands: a group keeps its direct members, and
//! a user's groups are found by walking up from the user ([`Groups::membership`]), so a file of nested groups costs
//! memory in proportion to its bytes, never to the product of its groups and users.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use super::glob::{self, Segment};
use super::ini::{self, Refused, Sink};
use super::{Access, AuthzFile, AuthzRefusal, AuthzRefused, Bounds, IgnoredEntry};

/// The section of group definitions.
/// Format: `groups_section[] = "groups"` (`authz_parse.c` 148).
const GROUPS: &[u8] = b"groups";

/// The section of alias definitions.
/// Format: `aliases_section[] = "aliases"` (`authz_parse.c` 145).
const ALIASES: &[u8] = b"aliases";

/// The one rule type token, which makes a section a glob rule.
/// Format: `glob_rule_token[] = "glob"` (`authz_parse.c` 151).
const GLOB_TOKEN: &[u8] = b"glob";

/// The byte that opens a type token and ends it and a repository name in a section's name.
/// Format: `if (*rule != ':')` and `memchr(rule, ':', rule_len)` (`authz_parse.c` 704-745).
const COLON: u8 = b':';

/// The bytes a group or alias name may not start with.
/// Format: `strchr("@$&*~", *option->data)` (`authz_parse.c` 351, 806).
const RESERVED_FIRST: &[u8] = b"@$&*~";

/// The byte that names a group in an entry or a member.
/// Format: `group = apr_pstrcat(cb->parser_pool, "@", option->data, ...)` (`authz_parse.c` 366).
const GROUP_SIGIL: u8 = b'@';

/// The byte that names an alias in an entry or a member.
/// Format: `alias = apr_pstrcat(cb->parser_pool, "&", option->data, ...)` (`authz_parse.c` 813).
const ALIAS_SIGIL: u8 = b'&';

/// The byte that inverts an entry.
/// Format: `const svn_boolean_t inverted = (*name == '~')` (`authz_parse.c` 850).
const INVERSION: u8 = b'~';

/// The entry for everyone, anonymous and authenticated.
/// Format: `case '*'` (`authz_parse.c` 875-889).
const EVERYONE: &[u8] = b"*";

/// The byte that opens a token entry.
/// Format: `case '$'` (`authz_parse.c` 891).
const TOKEN_SIGIL: u8 = b'$';

/// The entry for the anonymous user.
/// Format: `anon_access_token[] = "$anonymous"` (`authz_parse.c` 154).
const ANONYMOUS_TOKEN: &[u8] = b"$anonymous";

/// The entry for every authenticated user.
/// Format: `authn_access_token[] = "$authenticated"` (`authz_parse.c` 157).
const AUTHENTICATED_TOKEN: &[u8] = b"$authenticated";

/// The byte between a group's members.
/// Format: `svn_cstring_split(value->data, ",", TRUE, ...)` (`authz_parse.c` 391).
const MEMBER_SEPARATOR: u8 = b',';

/// The access mode byte for read.
/// Format: `case 'r'` (`authz_parse.c` 922).
const READ_MODE: u8 = b'r';

/// The access mode byte for write.
/// Format: `case 'w'` (`authz_parse.c` 926).
const WRITE_MODE: u8 = b'w';

/// A parsed configuration: its rules in file order with the root's default, its groups, the users it names, and the
/// entries it ignores.
#[derive(Clone, Debug)]
pub(super) struct Model {
  acls: Vec<Acl>,
  groups: Groups,
  known_users: BTreeSet<Box<[u8]>>,
  ignored: Vec<IgnoredEntry>,
}

/// One rule section (`authz.h` `authz_acl_t`).
#[derive(Clone, Debug)]
pub(super) struct Acl {
  /// The section's place among the rule sections, from one; the root's default is zero.
  pub(super) sequence: usize,
  /// The repository the section names; empty for every repository.
  pub(super) repository: Box<[u8]>,
  /// The path's segments; none for the root.
  pub(super) segments: Vec<Segment>,
  /// What the anonymous user is given, if any entry speaks for it (`$anonymous`, `*`, `~$authenticated`).
  pub(super) anonymous: Option<Access>,
  /// What every authenticated user is given, if any entry speaks for them (`$authenticated`, `*`, `~$anonymous`).
  pub(super) authenticated: Option<Access>,
  /// What the inverted entries give together, if there are any (SVN-4793): the global rights of a user the file never
  /// names.
  pub(super) inverted: Option<Access>,
  /// The entries for users and groups, aliases resolved, entries for empty groups dropped.
  pub(super) entries: Vec<Entry>,
}

/// An access entry for a user or a group.
#[derive(Clone, Debug)]
pub(super) struct Entry {
  /// Whom it names.
  pub(super) selector: Selector,
  /// Whether it applies to every authenticated user it does not name instead.
  pub(super) inverted: bool,
  /// What it gives.
  pub(super) access: Access,
}

/// Whom an entry names.
#[derive(Clone, Debug)]
pub(super) enum Selector {
  /// A user, by name (an alias's user too).
  User(Box<[u8]>),
  /// A group with members, by its place in [`Groups`].
  Group(usize),
}

/// The groups, each by its place in definition order: which groups hold each group, and which hold each user.
#[derive(Clone, Debug, Default)]
pub(super) struct Groups {
  /// For each group, the groups that name it as a member, once per naming.
  holders: Vec<Vec<usize>>,
  /// For each user named in a group (directly or through an alias), the groups that name the user.
  user_holders: BTreeMap<Box<[u8]>, Vec<usize>>,
}

impl Groups {
  /// For each group, whether `user` belongs to it, directly or through the groups it belongs to.
  pub(super) fn membership(&self, user: &[u8]) -> Vec<bool> {
    let mut member = vec![false; self.holders.len()];
    let mut pending: Vec<usize> = self.user_holders.get(user).cloned().unwrap_or_default();
    while let Some(group) = pending.pop() {
      let Some(slot) = member.get_mut(group) else {
        continue;
      };
      if *slot {
        continue;
      }
      *slot = true;
      if let Some(holders) = self.holders.get(group) {
        pending.extend_from_slice(holders);
      }
    }
    member
  }
}

impl Model {
  /// Reads the rules file and the global groups file, checks them whole, and builds the model.
  ///
  /// # Errors
  /// [`AuthzRefused`]: the first refusal in the rules file, then in the groups file, then the refusals found once both
  /// are read.
  pub(super) fn parse(
    rules: &[u8],
    groups: Option<&[u8]>,
    bounds: Bounds,
  ) -> Result<Self, AuthzRefused> {
    super::check_bytes(rules, AuthzFile::Rules, bounds)?;
    let mut builder = Builder::default();
    read_file(rules, AuthzFile::Rules, &mut builder)?;
    if let Some(groups) = groups {
      if let Some(first) = builder.groups.first() {
        return Err(AuthzRefused {
          file: AuthzFile::Rules,
          line: first.line,
          kind: AuthzRefusal::GroupsWithGroupsFile,
        });
      }
      super::check_bytes(groups, AuthzFile::Groups, bounds)?;
      builder.start_groups_file();
      read_file(groups, AuthzFile::Groups, &mut builder)?;
    }
    builder.finish()
  }

  /// The rule sections in file order, the root's default (sequence zero) last when the file has no global root rule.
  pub(super) fn acls(&self) -> &[Acl] {
    &self.acls
  }

  /// The groups.
  pub(super) fn groups(&self) -> &Groups {
    &self.groups
  }

  /// Whether the file names `user` anywhere: in an entry, as a group's member, or as an alias's user (Subversion keeps
  /// rights for exactly these users, `prepare_global_rights`).
  pub(super) fn knows(&self, user: &[u8]) -> bool {
    self.known_users.contains(user)
  }

  /// The entries ignored for naming an empty group.
  pub(super) fn ignored_entries(&self) -> &[IgnoredEntry] {
    &self.ignored
  }
}

/// Reads one file into `builder`, naming the file in its refusals.
fn read_file(bytes: &[u8], file: AuthzFile, builder: &mut Builder) -> Result<(), AuthzRefused> {
  ini::read(bytes, builder).map_err(|Refused { line, kind }| AuthzRefused { file, line, kind })
}

/// An access entry as read, before its names are resolved.
#[derive(Clone, Debug)]
struct RawEntry {
  line: usize,
  inverted: bool,
  /// The name after any `~`: a user, `@group` or `&alias`.
  name: Box<[u8]>,
  access: Access,
}

/// A rule section as read.
#[derive(Clone, Debug)]
struct RawAcl {
  sequence: usize,
  repository: Box<[u8]>,
  segments: Vec<Segment>,
  anonymous: Option<Access>,
  authenticated: Option<Access>,
  inverted: Option<Access>,
  entries: Vec<RawEntry>,
}

/// A group definition as read.
#[derive(Clone, Debug)]
struct GroupDefinition {
  file: AuthzFile,
  line: usize,
  /// The members as written, separators and whitespace removed.
  members: Vec<Box<[u8]>>,
}

/// What the section being read is.
#[derive(Clone, Copy, Debug, Default)]
enum Current {
  #[default]
  Nothing,
  Rule(usize),
  Groups,
  Aliases,
}

/// What an entry's name speaks for.
#[derive(Clone, Copy, Debug)]
enum Class {
  Everyone,
  Anonymous,
  Authenticated,
  Named,
}

/// The constructor's state while the files are read (`authz_parse.c` `ctor_baton_t`).
#[derive(Debug)]
struct Builder {
  file: AuthzFile,
  /// The section names seen in the file being read.
  sections: BTreeSet<Box<[u8]>>,
  /// The rules seen, by repository and path.
  rules: BTreeSet<(Box<[u8]>, Vec<Segment>)>,
  current: Current,
  acls: Vec<RawAcl>,
  groups: Vec<GroupDefinition>,
  group_index: BTreeMap<Box<[u8]>, usize>,
  aliases: BTreeMap<Box<[u8]>, Box<[u8]>>,
}

impl Default for Builder {
  fn default() -> Self {
    Self {
      file: AuthzFile::Rules,
      sections: BTreeSet::new(),
      rules: BTreeSet::new(),
      current: Current::Nothing,
      acls: Vec::new(),
      groups: Vec::new(),
      group_index: BTreeMap::new(),
      aliases: BTreeMap::new(),
    }
  }
}

impl<'a> Sink<'a> for Builder {
  fn section(&mut self, name: &'a [u8], _line: usize) -> Result<(), AuthzRefusal> {
    if !self.sections.insert(name.into()) {
      return Err(AuthzRefusal::SectionRepeated);
    }
    self.current = Current::Nothing;
    match self.file {
      AuthzFile::Rules => self.open_rules_section(name),
      AuthzFile::Groups if name == GROUPS => {
        self.current = Current::Groups;
        Ok(())
      }
      AuthzFile::Groups => Err(AuthzRefusal::SectionInvalid),
    }
  }

  fn option(
    &mut self,
    name: &'a [u8],
    value: Cow<'a, [u8]>,
    line: usize,
  ) -> Result<(), AuthzRefusal> {
    match self.current {
      Current::Groups => self.define_group(name, &value, line),
      Current::Aliases => self.define_alias(name, &value),
      Current::Rule(index) => self.add_entry(index, name, &value, line),
      Current::Nothing => Err(AuthzRefusal::SectionHeaderExpected),
    }
  }
}

impl Builder {
  /// Switches to the global groups file, whose section names are counted afresh (`apr_hash_clear(cb->sections)`).
  fn start_groups_file(&mut self) {
    self.file = AuthzFile::Groups;
    self.sections.clear();
    self.current = Current::Nothing;
  }

  /// A section of the rules file: a rule, `[aliases]` or `[groups]` (`rules_open_section`).
  fn open_rules_section(&mut self, name: &[u8]) -> Result<(), AuthzRefusal> {
    let (is_glob, rest) = rule_type(name)?;
    let (repository, path) = repository_of(rest)?;
    if path.first() == Some(&glob::SLASH) {
      return self.open_rule(is_glob, repository, path);
    }
    self.current = if name == ALIASES {
      Current::Aliases
    } else if name == GROUPS {
      Current::Groups
    } else {
      return Err(AuthzRefusal::SectionInvalid);
    };
    Ok(())
  }

  /// A rule section for `repository` (empty for every one) and `path`.
  fn open_rule(
    &mut self,
    is_glob: bool,
    repository: &[u8],
    path: &[u8],
  ) -> Result<(), AuthzRefusal> {
    let segments = glob::rule_path(path, is_glob)?;
    if !self.rules.insert((repository.into(), segments.clone())) {
      return Err(AuthzRefusal::RuleRepeated);
    }
    let index = self.acls.len();
    self.acls.push(RawAcl {
      sequence: index.saturating_add(1),
      repository: repository.into(),
      segments,
      anonymous: None,
      authenticated: None,
      inverted: None,
      entries: Vec::new(),
    });
    self.current = Current::Rule(index);
    Ok(())
  }

  /// A group definition (`groups_add_value`).
  fn define_group(&mut self, name: &[u8], value: &[u8], line: usize) -> Result<(), AuthzRefusal> {
    if reserved(name) {
      return Err(AuthzRefusal::GroupNameReserved);
    }
    if self.group_index.contains_key(name) {
      return Err(AuthzRefusal::GroupRedefined);
    }
    self.group_index.insert(name.into(), self.groups.len());
    self.groups.push(GroupDefinition {
      file: self.file,
      line,
      members: split_members(value),
    });
    Ok(())
  }

  /// An alias definition (`add_alias_definition`).
  fn define_alias(&mut self, name: &[u8], value: &[u8]) -> Result<(), AuthzRefusal> {
    if reserved(name) {
      return Err(AuthzRefusal::AliasNameReserved);
    }
    if self.aliases.contains_key(name) {
      return Err(AuthzRefusal::AliasRedefined);
    }
    self.aliases.insert(name.into(), value.into());
    Ok(())
  }

  /// An access entry of the rule section at `index` (`add_access_entry`).
  fn add_entry(
    &mut self,
    index: usize,
    name: &[u8],
    value: &[u8],
    line: usize,
  ) -> Result<(), AuthzRefusal> {
    let (inverted, rest) = match name.strip_prefix(&[INVERSION]) {
      Some(rest) => (true, rest),
      None => (false, name),
    };
    let class = classify_entry(rest, inverted)?;
    let access = parse_access(value)?;
    let acl = self
      .acls
      .get_mut(index)
      .ok_or(AuthzRefusal::SectionHeaderExpected)?;
    match class {
      Class::Everyone => {
        acl.anonymous = Some(grant(acl.anonymous, access));
        acl.authenticated = Some(grant(acl.authenticated, access));
      }
      Class::Anonymous => acl.anonymous = Some(grant(acl.anonymous, access)),
      Class::Authenticated => acl.authenticated = Some(grant(acl.authenticated, access)),
      Class::Named => {
        if inverted {
          acl.inverted = Some(grant(acl.inverted, access));
        }
        acl.entries.push(RawEntry {
          line,
          inverted,
          name: rest.into(),
          access,
        });
      }
    }
    Ok(())
  }

  /// The pass after reading: groups resolved and checked, entries resolved, the root's default in place.
  fn finish(self) -> Result<Model, AuthzRefused> {
    let resolved = self.resolve_groups()?;
    if let Some(first) = expands_forever(&resolved)
      .iter()
      .position(|forever| *forever)
    {
      let line = self.groups.get(first).map_or(1, |group| group.line);
      let file = self
        .groups
        .get(first)
        .map_or(AuthzFile::Rules, |group| group.file);
      return Err(AuthzRefused {
        file,
        line,
        kind: AuthzRefusal::GroupRecursive,
      });
    }
    let nonempty = nonempty_groups(&resolved);
    let mut known_users: BTreeSet<Box<[u8]>> = self.aliases.values().cloned().collect();
    known_users.extend(
      resolved
        .iter()
        .flat_map(|group| group.users.iter().cloned()),
    );
    let mut ignored = Vec::new();
    let mut acls = Vec::with_capacity(self.acls.len().saturating_add(1));
    for raw in &self.acls {
      acls.push(self.resolve_acl(raw, &nonempty, &mut known_users, &mut ignored)?);
    }
    Ok(Model {
      acls: with_root_default(acls),
      groups: index_groups(&resolved),
      known_users,
      ignored,
    })
  }

  /// Every group's members: users, aliases' users and groups (`expand_group_callback`'s lookups).
  fn resolve_groups(&self) -> Result<Vec<ResolvedGroup>, AuthzRefused> {
    self
      .groups
      .iter()
      .map(|definition| self.resolve_group(definition))
      .collect()
  }

  /// One group's members, a refusal naming the group's line.
  fn resolve_group(&self, definition: &GroupDefinition) -> Result<ResolvedGroup, AuthzRefused> {
    let mut group = ResolvedGroup::default();
    for member in &definition.members {
      let resolved = self.resolve_member(member).map_err(|kind| AuthzRefused {
        file: definition.file,
        line: definition.line,
        kind,
      })?;
      match resolved {
        Selector::User(user) => group.users.push(user),
        Selector::Group(nested) => group.nested.push(nested),
      }
    }
    Ok(group)
  }

  /// A group's member: `&alias` is its user, `@group` that group, anything else a user's name.
  fn resolve_member(&self, member: &[u8]) -> Result<Selector, AuthzRefusal> {
    match member.split_first() {
      Some((&ALIAS_SIGIL, alias)) => self.alias_user(alias).map(Selector::User),
      Some((&GROUP_SIGIL, name)) => self
        .group_index
        .get(name)
        .copied()
        .map(Selector::Group)
        .ok_or(AuthzRefusal::GroupUndefined),
      _ => Ok(Selector::User(member.into())),
    }
  }

  /// The user an alias stands for.
  fn alias_user(&self, alias: &[u8]) -> Result<Box<[u8]>, AuthzRefusal> {
    self
      .aliases
      .get(alias)
      .cloned()
      .ok_or(AuthzRefusal::AliasUndefined)
  }

  /// Whom an entry names; `None` for a group without members, which Subversion ignores (`array_insert_ace`).
  fn resolve_entry(
    &self,
    entry: &RawEntry,
    nonempty: &[bool],
  ) -> Result<Option<Selector>, AuthzRefusal> {
    match entry.name.split_first() {
      Some((&ALIAS_SIGIL, alias)) => self
        .alias_user(alias)
        .map(|user| Some(Selector::User(user))),
      Some((&GROUP_SIGIL, name)) => {
        let group = self
          .group_index
          .get(name)
          .copied()
          .ok_or(AuthzRefusal::EntryGroupUndefined)?;
        Ok(
          nonempty
            .get(group)
            .copied()
            .unwrap_or(false)
            .then_some(Selector::Group(group)),
        )
      }
      _ => Ok(Some(Selector::User(entry.name.clone()))),
    }
  }

  /// A rule section with its entries' names resolved (`merge_alias_ace`, `array_insert_ace`).
  fn resolve_acl(
    &self,
    raw: &RawAcl,
    nonempty: &[bool],
    known_users: &mut BTreeSet<Box<[u8]>>,
    ignored: &mut Vec<IgnoredEntry>,
  ) -> Result<Acl, AuthzRefused> {
    let mut entries = Vec::with_capacity(raw.entries.len());
    for entry in &raw.entries {
      let resolved = self
        .resolve_entry(entry, nonempty)
        .map_err(|kind| AuthzRefused {
          file: AuthzFile::Rules,
          line: entry.line,
          kind,
        })?;
      let Some(selector) = resolved else {
        ignored.push(IgnoredEntry { line: entry.line });
        continue;
      };
      if let Selector::User(user) = &selector {
        known_users.insert(user.clone());
      }
      entries.push(Entry {
        selector,
        inverted: entry.inverted,
        access: entry.access,
      });
    }
    Ok(Acl {
      sequence: raw.sequence,
      repository: raw.repository.clone(),
      segments: raw.segments.clone(),
      anonymous: raw.anonymous,
      authenticated: raw.authenticated,
      inverted: raw.inverted,
      entries,
    })
  }
}

/// A group's members once resolved.
#[derive(Clone, Debug, Default)]
struct ResolvedGroup {
  /// Its users, aliases' users among them.
  users: Vec<Box<[u8]>>,
  /// The groups it names, by place, once per naming.
  nested: Vec<usize>,
}

/// For each group, the groups that name it, once per naming.
fn holders_of(groups: &[ResolvedGroup]) -> Vec<Vec<usize>> {
  let mut holders = vec![Vec::new(); groups.len()];
  for (holder, group) in groups.iter().enumerate() {
    for nested in &group.nested {
      if let Some(slot) = holders.get_mut(*nested) {
        slot.push(holder);
      }
    }
  }
  holders
}

/// For each group, whether expanding it never ends: it lies on a cycle of groups or names one that leads into a cycle.
/// Groups whose members all end are peeled away until none is left to peel (module documentation of `authz`, item 5).
fn expands_forever(groups: &[ResolvedGroup]) -> Vec<bool> {
  let holders = holders_of(groups);
  let mut unfinished: Vec<usize> = groups.iter().map(|group| group.nested.len()).collect();
  let mut finished: Vec<usize> = unfinished
    .iter()
    .enumerate()
    .filter(|(_, count)| **count == 0)
    .map(|(group, _)| group)
    .collect();
  let mut done = vec![false; groups.len()];
  while let Some(group) = finished.pop() {
    if let Some(slot) = done.get_mut(group) {
      *slot = true;
    }
    for holder in holders.get(group).map_or(&[][..], Vec::as_slice) {
      if let Some(count) = unfinished.get_mut(*holder) {
        *count = count.saturating_sub(1);
        if *count == 0 {
          finished.push(*holder);
        }
      }
    }
  }
  done.iter().map(|finished| !*finished).collect()
}

/// For each group, whether it has a member: a user, or a group that has one. Groups form no cycle by now.
fn nonempty_groups(groups: &[ResolvedGroup]) -> Vec<bool> {
  let holders = holders_of(groups);
  let mut nonempty = vec![false; groups.len()];
  let mut pending: Vec<usize> = groups
    .iter()
    .enumerate()
    .filter(|(_, group)| !group.users.is_empty())
    .map(|(group, _)| group)
    .collect();
  while let Some(group) = pending.pop() {
    let Some(slot) = nonempty.get_mut(group) else {
      continue;
    };
    if *slot {
      continue;
    }
    *slot = true;
    if let Some(holders) = holders.get(group) {
      pending.extend_from_slice(holders);
    }
  }
  nonempty
}

/// The groups as membership asks of them.
fn index_groups(groups: &[ResolvedGroup]) -> Groups {
  let mut user_holders: BTreeMap<Box<[u8]>, Vec<usize>> = BTreeMap::new();
  for (group, resolved) in groups.iter().enumerate() {
    for user in &resolved.users {
      user_holders.entry(user.clone()).or_default().push(group);
    }
  }
  Groups {
    holders: holders_of(groups),
    user_holders,
  }
}

/// `acls` with the root's default in place (`insert_default_acl` and its merge in `svn_authz__parse`): a global root
/// rule speaks for the anonymous and every authenticated user, giving them nothing unless it says otherwise; without
/// one, a rule of sequence zero at the root gives everybody nothing.
fn with_root_default(mut acls: Vec<Acl>) -> Vec<Acl> {
  let root = acls
    .iter_mut()
    .find(|acl| acl.repository.is_empty() && acl.segments.is_empty());
  match root {
    Some(root) => {
      root.anonymous = Some(root.anonymous.unwrap_or(Access::None));
      root.authenticated = Some(root.authenticated.unwrap_or(Access::None));
    }
    None => acls.push(Acl {
      sequence: 0,
      repository: Box::default(),
      segments: Vec::new(),
      anonymous: Some(Access::None),
      authenticated: Some(Access::None),
      inverted: Some(Access::None),
      entries: Vec::new(),
    }),
  }
  acls
}

/// Whether a group or alias name starts with a byte Subversion reserves.
fn reserved(name: &[u8]) -> bool {
  name
    .first()
    .is_some_and(|first| RESERVED_FIRST.contains(first))
}

/// A group's members: split at commas, each stripped, the empty ones dropped (`svn_cstring_split`).
fn split_members(value: &[u8]) -> Vec<Box<[u8]>> {
  value
    .split(|byte| *byte == MEMBER_SEPARATOR)
    .map(ini::strip)
    .filter(|member| !member.is_empty())
    .map(Box::from)
    .collect()
}

/// A section name's type token: `(true, rest)` after `:glob:`, `(false, name)` without a token.
fn rule_type(name: &[u8]) -> Result<(bool, &[u8]), AuthzRefusal> {
  let Some(rest) = name.strip_prefix(&[COLON]) else {
    return Ok((false, name));
  };
  let end = rest
    .iter()
    .position(|byte| *byte == COLON)
    .ok_or(AuthzRefusal::RepositoryNameEmpty)?;
  if rest.get(..end) != Some(GLOB_TOKEN) {
    return Err(AuthzRefusal::RuleTypeInvalid);
  }
  Ok((true, rest.get(end.saturating_add(1)..).unwrap_or_default()))
}

/// A section name's repository (empty for none) and the rest after it.
fn repository_of(rest: &[u8]) -> Result<(&[u8], &[u8]), AuthzRefusal> {
  if rest.first() == Some(&glob::SLASH) {
    return Ok((&[], rest));
  }
  match rest.iter().position(|byte| *byte == COLON) {
    None => Ok((&[], rest)),
    Some(0) => Err(AuthzRefusal::RepositoryNameEmpty),
    Some(end) => Ok((
      rest.get(..end).unwrap_or_default(),
      rest.get(end.saturating_add(1)..).unwrap_or_default(),
    )),
  }
}

/// What an entry's name, after any `~`, speaks for.
fn classify_entry(name: &[u8], inverted: bool) -> Result<Class, AuthzRefusal> {
  match name.first() {
    Some(&INVERSION) => Err(AuthzRefusal::EntryDoublyInverted),
    Some(_) if name.starts_with(EVERYONE) => {
      if name != EVERYONE {
        Err(AuthzRefusal::EntryStarNotAlone)
      } else if inverted {
        Err(AuthzRefusal::EntryInvertedStar)
      } else {
        Ok(Class::Everyone)
      }
    }
    Some(&TOKEN_SIGIL) => classify_token(name, inverted),
    _ => Ok(Class::Named),
  }
}

/// A `$` entry: `$anonymous` or `$authenticated`, swapped by `~`.
fn classify_token(name: &[u8], inverted: bool) -> Result<Class, AuthzRefusal> {
  match (name, inverted) {
    (ANONYMOUS_TOKEN, false) | (AUTHENTICATED_TOKEN, true) => Ok(Class::Anonymous),
    (AUTHENTICATED_TOKEN, false) | (ANONYMOUS_TOKEN, true) => Ok(Class::Authenticated),
    _ => Err(AuthzRefusal::EntryTokenInvalid),
  }
}

/// An entry's value: `r`, `w` and whitespace in any order; write alone is refused.
fn parse_access(value: &[u8]) -> Result<Access, AuthzRefusal> {
  let mut read = false;
  let mut write = false;
  for byte in value {
    match *byte {
      READ_MODE => read = true,
      WRITE_MODE => write = true,
      other if ini::is_space(other) => {}
      _ => return Err(AuthzRefusal::AccessModeInvalid),
    }
  }
  match (read, write) {
    (false, false) => Ok(Access::None),
    (true, false) => Ok(Access::Read),
    (true, true) => Ok(Access::Write),
    (false, true) => Err(AuthzRefusal::AccessWriteOnly),
  }
}

/// `held` raised by `access`: entries that speak for the same principal combine.
fn grant(held: Option<Access>, access: Access) -> Access {
  held.map_or(access, |held| held.max(access))
}
