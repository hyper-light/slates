//! Subversion's path-based authorization (§4.17.4 "10. Who may do what" and piece 10; A-606): the authz file that
//! `svnserve`'s `authz-db` and `mod_authz_svn`'s `AuthzSVNAccessFile` name, parsed as Subversion parses it and evaluated
//! with Subversion's precedence, giving each principal read, write or no access at each path.
//!
//! The evidence is Subversion's `1.14.x` branch at `d434dc14` (`libsvn_repos/authz_parse.c`, `authz.c`,
//! `authz_info.c` and `authz.h`; `libsvn_subr/config_file.c` for the file's lines), its tests
//! (`tests/libsvn_repos/authz-test.c`, `authz.rules`, `authz.groups`, `repos-test.c`), APR 1.7.5's `apr_fnmatch.c` and
//! its tests, the 1.10 and 1.14 release notes and the book's path-based authorization page;
//! `docs/wip/research/vcs-servers-subversion.md` §7 lists them with their hashes. Subversion's trunk at `fd4e82b`
//! (2026-11-07) has the same lookup, and its parser differs in comments alone.
//!
//! **The file** (`config_file.c` `svn_config__parse_stream`): an optional UTF-8 byte order mark, then lines. A comment
//! (`#`) and a section header (`[`) start in the first column; a header ends at its first `]` and the rest of its line
//! is ignored. An option is `name = value` or `name: value`, a `:` before the first `=` being the separator. A line that
//! starts with whitespace continues the value above it, joined with one space, and a blank line ends that value. A
//! carriage return that starts a line or follows `[` is dropped; anywhere else it is whitespace or a byte of a name.
//! Names are compared byte for byte, case and all.
//!
//! **The sections** (`authz_parse.c`): `[groups]` (`name = member, member`, a member being a user, `@group` or
//! `&alias`), `[aliases]` (`name = user`), and rules: `[repository:/path]` and `[/path]`, matched literally, and from
//! 1.10 `[:glob:repository:/path]` and `[:glob:/path]`, whose segments may be `*` (one segment), `**` (any number of
//! segments, none included), a prefix `abc*`, a suffix `*abc` or any other `apr_fnmatch` pattern. A rule's entries name
//! a user, `@group`, `&alias`, `$anonymous`, `$authenticated` or `*` (both), each optionally inverted by `~`
//! (everyone the name does not match, among the authenticated), with `r`, `rw` or nothing. The entries of one section
//! that match a principal combine, so the most any of them grants is granted (1.10). A separate groups file (the
//! `groups-db` option) may hold the `[groups]` section instead, and then the rules file may define no group.
//!
//! **The precedence** (`authz.c`): for a principal in a repository, a rule applies when its section names that
//! repository or none and an entry of it matches the principal. Where a global section and the repository's own
//! section have the same path, the repository's applies if it matches the principal and the global one otherwise
//! (1.14.1, issue 4762). At a path, the deepest of its levels (the root, then each segment in turn) that an applying
//! rule matches decides, and among the rules matching that level the one defined last in the file wins (1.10). A rule
//! ending in `**` eclipses every rule beneath its own prefix that was defined before it (`trim_tree`). Without a rule at
//! the root, nobody may do anything there. A recursive question (`svn_authz_recursive`: may the principal do this at
//! the path and everywhere beneath it?) is answered by the least access among the path's own and that of every rule
//! that could govern a path beneath it, whether or not such a path exists, as Subversion answers it. A question
//! without a path (anywhere in the repository?) is answered by the most access any rule for the principal grants
//! (`svn_authz__get_global_rights`).
//!
//! **Where Subversion's code and its own documentation part**, this module follows the documentation, and each case
//! is a named test: (1) the root has no segments, where `lookup` walks `/` as one empty segment, so that `[:glob:/*]`
//! and its kin decide the root's access there although `*` "Matches any single non-empty path segment" (`authz.h`);
//! (2) so a recursive question at the root weighs every rule, where Subversion's weighs only the rules an empty segment
//! matches and grants recursive access at a root above a denied subtree; (3) matching a suffix pattern leaves the
//! segment as it was, where `lookup` reverses the segment in place for the rest of its level, so another node's rules
//! at that level meet it reversed; (4) a section whose path starts with two slashes is refused as the non-canonical
//! path it is ("A path with two consecutive slashes is not canonical"), where `parse_rule_path` takes it for the root
//! and ignores the rest. Where Subversion's outcome depends on hash order or on bytes past its buffer, this module
//! decides one way: (5) a group whose membership leads back to itself through other groups is refused whichever group
//! is met first, where Subversion refuses only a cycle through the group it happens to expand first and otherwise
//! recurses until its stack runs out; (6) an option with nothing before its separator is refused; (7) a NUL byte is
//! refused, where Subversion's C strings cut a name or a header at it; (8) a file with several errors is refused at the
//! first in file order, and a refusal found after the reading (an undefined group, a cycle) at the line of the
//! definition or entry that names it.
//!
//! **Bounds**: one, given by the caller ([`Bounds`]): the bytes a file may hold, checked before anything is read; every
//! structure built is proportional to the bytes read. Nothing recurses: groups, rule paths and the rule tree are walked
//! with explicit stacks, so a deep file costs memory within the bound and never the stack. Every refusal is typed, with
//! the line it was found on and the number of the Subversion error a client is answered with ([`AuthzRefused`]).
//!
//! No I/O, no clock, no randomness, no panic.

mod glob;
mod ini;
mod model;
mod tree;

#[cfg(test)]
mod tests;

pub use tree::PrincipalRules;

/// `SVN_ERR_MALFORMED_FILE`, which `svn_config__parse_stream` answers for a line it cannot read.
/// Format: the miscellaneous category's start, 120,000 + 16 × 5,000 = 200,000, plus 2 (`svn_error_codes.h` 1289-1290).
const SVN_ERR_MALFORMED_FILE: u32 = 200_002;

/// `SVN_ERR_AUTHZ_INVALID_CONFIG`, which the authz parser answers for every other refusal.
/// Format: the authz category's start, 120,000 + 20 × 5,000 = 220,000, plus 3 (`svn_error_codes.h` 1651-1652).
const SVN_ERR_AUTHZ_INVALID_CONFIG: u32 = 220_003;

/// What a principal may do at a path: Subversion's three levels, in order (`authz.h` `authz_access_t`: write implies
/// read, and write alone does not exist).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
  /// Nothing: to the principal the path is absent.
  None,
  /// Read: list directories, read texts and properties.
  Read,
  /// Read and write.
  Write,
}

/// Who asks. Subversion takes a missing name and the empty name alike for the anonymous user (`authz.h`
/// `AUTHZ_ANONYMOUS_USER`), and so does every evaluation here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Principal<'a> {
  /// No authenticated name.
  Anonymous,
  /// An authenticated user's name, compared byte for byte.
  User(&'a [u8]),
}

impl<'a> Principal<'a> {
  /// The principal Subversion takes `name` for: no name, or the empty one, is anonymous.
  pub fn named(name: Option<&'a [u8]>) -> Self {
    match name {
      Some(user) if !user.is_empty() => Self::User(user),
      _ => Self::Anonymous,
    }
  }

  /// The principal as evaluated: the empty name is anonymous.
  fn evaluated(self) -> Self {
    match self {
      Self::User([]) => Self::Anonymous,
      other => other,
    }
  }
}

/// The bound a file is read within, given by the caller (module documentation): the server derives it from the share of
/// its shard's memory the repository's configuration may take, as it derives its other caps (§4.17.4 item 12).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
  /// The most bytes the rules file, and the groups file, may each hold.
  pub file_bytes: usize,
}

impl Bounds {
  /// The tightest bound that admits `rules` and `groups`: the longer one's length. This is how a bound is derived from
  /// the data it governs.
  pub fn of(rules: &[u8], groups: Option<&[u8]>) -> Self {
    Self {
      file_bytes: rules.len().max(groups.map_or(0, <[u8]>::len)),
    }
  }
}

/// Which file a refusal was found in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthzFile {
  /// The rules file (`authz-db`, `AuthzSVNAccessFile`).
  Rules,
  /// The global groups file (`groups-db`, `AuthzSVNGroupsFile`).
  Groups,
}

/// Why a file was refused: one kind for each distinct refusal Subversion makes (its message quoted), and the few this
/// module adds (module documentation, items 4 to 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthzRefusal {
  /// The file holds more bytes than [`Bounds::file_bytes`].
  TooLarge,
  /// A NUL byte, which Subversion's C strings would cut a name or a header at.
  NulByte,
  /// "Section header must start in the first column".
  SectionHeaderIndented,
  /// "Comment must start in the first column".
  CommentIndented,
  /// "Section header expected": an option before any section.
  SectionHeaderExpected,
  /// "Option expected": an indented line that continues no value.
  OptionExpected,
  /// "Option must end with ':' or '='".
  OptionSeparatorMissing,
  /// An option with nothing before its separator, whose name Subversion reads from before its buffer.
  OptionNameEmpty,
  /// "Section header must end with ']'".
  SectionHeaderUnterminated,
  /// "Section appears more than once in the authz file" (or in the global groups file).
  SectionRepeated,
  /// "Section is not valid in the authz file" (or in the global group file): neither a rule, `[groups]` nor
  /// `[aliases]`.
  SectionInvalid,
  /// "Empty repository name in authz rule": `[:/path]`, `[:glob:]`-style tokens without their second `:`, or `[:path]`.
  RepositoryNameEmpty,
  /// "Invalid type token ... in authz rule": a token between the first two colons other than `glob`.
  RuleTypeInvalid,
  /// "Non-canonical path ... Found empty name in authz rule path": two slashes in a row, or a path ending in one.
  RulePathEmptySegment,
  /// "Non-canonical path ... Found '.' in authz rule path".
  RulePathDot,
  /// "Non-canonical path ... Found '..' in authz rule path".
  RulePathDotDot,
  /// "Section ... describes the same rule as section ...": two sections of one repository whose paths are one rule.
  RuleRepeated,
  /// "Group name ... may not begin with ...": a group named from `@`, `$`, `&`, `*` or `~`.
  GroupNameReserved,
  /// "Can't override definition of group" (or of global group).
  GroupRedefined,
  /// "Alias name ... may not begin with ...".
  AliasNameReserved,
  /// "Can't override definition of alias".
  AliasRedefined,
  /// "Access entry ... has more than one inversion; double negatives are not permitted".
  EntryDoublyInverted,
  /// "Access entry ... is not valid; it must be a single '*'".
  EntryStarNotAlone,
  /// "Access entry '~*' will never match".
  EntryInvertedStar,
  /// "Access entry token ... is not valid; should be '$anonymous' or '$authenticated'".
  EntryTokenInvalid,
  /// "The access mode ... in access entry ... of rule ... is not valid": a byte other than `r`, `w` and whitespace.
  AccessModeInvalid,
  /// "Write-only access entry ... of rule ... is not valid".
  AccessWriteOnly,
  /// "Authz file cannot contain any groups when global groups are being used."
  GroupsWithGroupsFile,
  /// "Alias ... was never defined": in a group or in an access entry.
  AliasUndefined,
  /// "Undefined group": a group's member names a group nowhere defined.
  GroupUndefined,
  /// "Access entry refers to undefined group".
  EntryGroupUndefined,
  /// "Recursive definition of group": the group's membership leads back to itself.
  GroupRecursive,
}

/// A refused file: where, at which line (counted from one), and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthzRefused {
  /// The file the refusal was found in.
  pub file: AuthzFile,
  /// The line of the header, option, definition or entry refused; for [`AuthzRefusal::TooLarge`], one.
  pub line: usize,
  /// Why.
  pub kind: AuthzRefusal,
}

impl AuthzRefused {
  /// The number of the Subversion error this refusal is answered with, which `svn` prints after an `E`.
  pub fn subversion_code(self) -> u32 {
    match self.kind {
      AuthzRefusal::NulByte
      | AuthzRefusal::SectionHeaderIndented
      | AuthzRefusal::CommentIndented
      | AuthzRefusal::SectionHeaderExpected
      | AuthzRefusal::OptionExpected
      | AuthzRefusal::OptionSeparatorMissing
      | AuthzRefusal::OptionNameEmpty
      | AuthzRefusal::SectionHeaderUnterminated => SVN_ERR_MALFORMED_FILE,
      _ => SVN_ERR_AUTHZ_INVALID_CONFIG,
    }
  }
}

/// An access entry for a group with no members, which Subversion ignores (1.12 and later: "Ignoring access entry for
/// empty group"); `svnauthz` prints it as a warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IgnoredEntry {
  /// The entry's line in the rules file.
  pub line: usize,
}

/// What a principal may do anywhere in a repository, as Subversion accumulates it from every rule for the principal
/// (`authz_info.c` `svn_authz__get_global_rights`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlobalRights {
  /// The least access the rules give: at most what the principal may do at every path.
  pub every_path: Access,
  /// The most access the rules give: what the principal may do at some path, at most.
  pub some_path: Access,
  /// Whether a rule names the repository itself, or the question named no repository.
  pub explicit: bool,
}

/// A path in a repository, as authorization is asked about it: it starts with `/`; runs of `/` and a trailing `/` are
/// read as one and none, as Subversion's `lookup` reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepositoryPath<'a> {
  path: &'a [u8],
}

/// Why a path was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRefused {
  /// It does not start with `/` (Subversion asserts it does: `svn_repos_authz_check_access`).
  NotAbsolute,
  /// It holds a NUL byte, which no Subversion path may (`svn_path_check_valid`).
  NulByte,
}

impl<'a> RepositoryPath<'a> {
  /// The path `path`, if it is one.
  ///
  /// # Errors
  /// [`PathRefused`] when it does not start with `/` or holds a NUL byte.
  pub fn new(path: &'a [u8]) -> Result<Self, PathRefused> {
    if path.first() != Some(&glob::SLASH) {
      return Err(PathRefused::NotAbsolute);
    }
    if path.contains(&ini::NUL) {
      return Err(PathRefused::NulByte);
    }
    Ok(Self { path })
  }

  /// The path's segments, root first; the root has none.
  fn segments(self) -> impl Iterator<Item = &'a [u8]> {
    self
      .path
      .split(|byte| *byte == glob::SLASH)
      .filter(|segment| !segment.is_empty())
  }
}

/// A parsed authz configuration: its rules, groups and aliases, checked whole (module documentation).
#[derive(Clone, Debug)]
pub struct Authz {
  model: model::Model,
}

impl Authz {
  /// Parses the rules file `rules` and, when given, the global groups file `groups`, each within `bounds`.
  ///
  /// # Errors
  /// [`AuthzRefused`] for the first refusal in file order, then for those found after reading (module documentation,
  /// item 8).
  pub fn parse(rules: &[u8], groups: Option<&[u8]>, bounds: Bounds) -> Result<Self, AuthzRefused> {
    Ok(Self {
      model: model::Model::parse(rules, groups, bounds)?,
    })
  }

  /// The access entries ignored because their group has no members, in file order.
  pub fn ignored_entries(&self) -> &[IgnoredEntry] {
    self.model.ignored_entries()
  }

  /// What `principal` may do anywhere in `repository` (`b""` asks about rules that name no repository), as Subversion
  /// accumulates it.
  pub fn global_rights(&self, repository: &[u8], principal: Principal<'_>) -> GlobalRights {
    tree::global_rights(&self.model, repository, principal.evaluated())
  }

  /// The rules for `principal` in `repository` (`b""`: a question that names no repository, which only rules naming
  /// none answer), compiled once to answer every path.
  pub fn rules(&self, repository: &[u8], principal: Principal<'_>) -> PrincipalRules {
    PrincipalRules::compile(&self.model, repository, principal.evaluated())
  }
}

/// Refuses a file past the bound or holding a NUL byte, before anything is read from it; the model calls it for each
/// file as it comes to read it, the groups file after the rules file.
fn check_bytes(bytes: &[u8], file: AuthzFile, bounds: Bounds) -> Result<(), AuthzRefused> {
  if bytes.len() > bounds.file_bytes {
    return Err(AuthzRefused {
      file,
      line: 1,
      kind: AuthzRefusal::TooLarge,
    });
  }
  match bytes.iter().position(|byte| *byte == ini::NUL) {
    None => Ok(()),
    Some(at) => Err(AuthzRefused {
      file,
      line: ini::line_of(bytes, at),
      kind: AuthzRefusal::NulByte,
    }),
  }
}
