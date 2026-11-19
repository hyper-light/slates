//! The authz module's tests (A-606; §4.17.4 item 10 and piece 10). Golden vectors: Subversion's own authz test cases
//! (`tests/libsvn_repos/authz-test.c`, `authz.rules`, `authz.groups` and the authz tests of `repos-test.c`, Subversion's
//! `1.14.x` branch at `d434dc14`, each cited by function), APR 1.7.5's `apr_fnmatch` table, and the files and stated
//! outcomes of Subversion's documentation (`svnadmin create`'s `conf/authz`, the book's path-based authorization page,
//! the 1.10 and 1.14 release notes). Then every refusal by kind and line, the module documentation's divergences each
//! as a named test, the uniform fast path's counter, hostile input, and the naive oracle (`oracle`).

use super::*;
use proptest::prelude::*;

mod oracle;

/// Parses `rules` with no groups file, within the tightest bound.
fn parse(rules: &str) -> Authz {
  Authz::parse(rules.as_bytes(), None, Bounds::of(rules.as_bytes(), None)).expect("the file parses")
}

/// Parses `rules` with the global groups file `groups`, within the tightest bound.
fn parse_with_groups(rules: &str, groups: &str) -> Authz {
  Authz::parse(
    rules.as_bytes(),
    Some(groups.as_bytes()),
    Bounds::of(rules.as_bytes(), Some(groups.as_bytes())),
  )
  .expect("the files parse")
}

/// The refusal of `rules`, with no groups file.
fn refusal(rules: &[u8]) -> AuthzRefused {
  Authz::parse(rules, None, Bounds::of(rules, None)).expect_err("the file is refused")
}

/// The refusal of `rules` with the groups file `groups`.
fn refusal_with_groups(rules: &[u8], groups: &[u8]) -> AuthzRefused {
  Authz::parse(rules, Some(groups), Bounds::of(rules, Some(groups)))
    .expect_err("the files are refused")
}

/// A refusal in the rules file.
fn in_rules(line: usize, kind: AuthzRefusal) -> AuthzRefused {
  AuthzRefused {
    file: AuthzFile::Rules,
    line,
    kind,
  }
}

/// A refusal in the groups file.
fn in_groups(line: usize, kind: AuthzRefusal) -> AuthzRefused {
  AuthzRefused {
    file: AuthzFile::Groups,
    line,
    kind,
  }
}

/// The path `path`.
fn path(path: &str) -> RepositoryPath<'_> {
  RepositoryPath::new(path.as_bytes()).expect("an absolute path")
}

/// The principal `user` names, `None` being anonymous.
fn principal(user: Option<&str>) -> Principal<'_> {
  Principal::named(user.map(str::as_bytes))
}

/// What a check requires, as `svn_repos_authz_check_access`'s `required_access` names it.
#[derive(Clone, Copy, Debug)]
enum Need {
  Read,
  Write,
  ReadBeneath,
  WriteBeneath,
}

/// One of `repos-test.c`'s `check_access_tests`: the path (`None`, any path), the repository (`None`, any), the user
/// (`None`, anonymous), what is required, and whether Subversion's test expects it granted.
type Case = (
  Option<&'static str>,
  Option<&'static str>,
  Option<&'static str>,
  Need,
  bool,
);

/// Whether `svn_repos_authz_check_access` grants `need` at `at` to `user` in `repository`, answered here.
fn granted(
  authz: &Authz,
  at: Option<&str>,
  repository: Option<&str>,
  user: Option<&str>,
  need: Need,
) -> bool {
  let rules = authz.rules(repository.unwrap_or("").as_bytes(), principal(user));
  let (required, beneath) = match need {
    Need::Read => (Access::Read, false),
    Need::Write => (Access::Write, false),
    Need::ReadBeneath => (Access::Read, true),
    Need::WriteBeneath => (Access::Write, true),
  };
  let held = match at {
    None => rules.access_anywhere(),
    Some(at) if beneath => rules.access_beneath(path(at)),
    Some(at) => rules.access(path(at)),
  };
  held >= required
}

/// Every case of `cases` answered as Subversion's test expects.
fn check_cases(authz: &Authz, cases: &[Case]) {
  for (index, (at, repository, user, need, expected)) in cases.iter().enumerate() {
    assert_eq!(
      granted(authz, *at, *repository, *user, *need),
      *expected,
      "case {index}: {need:?} at {at:?} in {repository:?} for {user:?}"
    );
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// Subversion's own test cases: authz-test.c.

/// `authz.rules` (`tests/libsvn_repos/`, sha256 `faed4da16fc20b00`), byte for byte: its lines end in CR LF.
const AUTHZ_RULES: &[u8] = b"[/]\r\n~$anonymous = rw\r\n~$authenticated = r\r\n&x = r\r\nluser = rw\r\n\r\n\
[bloop:/]\r\n@x = r\r\n@y = rw\r\n\r\n\
[:glob:bloop:/*a\\bc/def*/**/*/**/ghi?jkl/**/**/mno\\/**]\r\n$anonymous =\r\n~luser =\r\n~@x = r\r\nluser = rw\r\n\r\n\
[aliases]\r\nx = luser\r\n";

/// `authz.groups` (`tests/libsvn_repos/`, sha256 `7436b89c876dd1e8`), byte for byte.
const AUTHZ_GROUPS: &[u8] = b"[groups]\nx = a\ny = b, @z\nz = c, @x\n";

/// `test_authz_parse`: Subversion's rules and groups files parse.
#[test]
fn test_authz_parse_reads_subversions_own_rules_and_groups_files() {
  let authz = Authz::parse(
    AUTHZ_RULES,
    Some(AUTHZ_GROUPS),
    Bounds::of(AUTHZ_RULES, Some(AUTHZ_GROUPS)),
  );
  assert!(authz.is_ok(), "{authz:?}");
}

/// The same files, asked what `test_authz_parse` prints and more, each answer derived by hand from the rules in the
/// module documentation (not Subversion's expectations: its test prints and asserts nothing).
#[test]
fn subversions_own_rules_file_answers_as_its_rules_say() {
  let authz = Authz::parse(
    AUTHZ_RULES,
    Some(AUTHZ_GROUPS),
    Bounds::of(AUTHZ_RULES, Some(AUTHZ_GROUPS)),
  )
  .expect("parses");
  // The global rights `test_authz_parse` prints for ('wunga', 'bloop'): an unnamed user, through the inverted entries
  // of the glob rule (r) and every authenticated user's root rule (rw).
  assert_eq!(
    authz.global_rights(b"bloop", principal(Some("wunga"))),
    GlobalRights {
      every_path: Access::Read,
      some_path: Access::Write,
      explicit: true,
    }
  );
  // The glob rule's path, normalized as Subversion normalizes it: suffix `abc` (the escape dropped), prefix `def`, one
  // segment, any, the pattern `ghi?jkl`, any, the literal `mno\` (a trailing backslash kept), any.
  let deep = "/xabc/defx/q/ghiXjkl/mno\\/z";
  let at = |user: Option<&str>| authz.rules(b"bloop", principal(user)).access(path(deep));
  assert_eq!(at(Some("luser")), Access::Write, "luser = rw, and ~@x");
  assert_eq!(
    at(Some("a")),
    Access::None,
    "a is in @x: only ~luser = (nothing) applies"
  );
  assert_eq!(at(Some("wunga")), Access::Read, "~luser and ~@x = r");
  assert_eq!(at(None), Access::None, "$anonymous =");
  let root = |user: Option<&str>| authz.rules(b"bloop", principal(user)).access(path("/"));
  assert_eq!(
    root(Some("a")),
    Access::Write,
    "[bloop:/] applies to a: @x = r, @y (b, c, a) = rw"
  );
  assert_eq!(
    root(Some("luser")),
    Access::Write,
    "[bloop:/] does not apply to luser: [/] does"
  );
  assert_eq!(root(None), Access::Read, "[/] ~$authenticated = r");
}

/// `test_global_rights`, its three files and every case: `svn_authz__get_global_rights`' least and most access and
/// whether they were explicit.
#[test]
fn test_global_rights_answers_every_case_of_subversions_test() {
  use Access::{None as N, Read as R, Write as W};
  let authz1 = "[/public]\n* = r\n\n[greek:/A]\nuserA = rw\n\n[repo:/A]\nuserA = r\n\n[repo:/B]\nuserA = rw\n\n\
[greek:/B]\nuserB = rw\n";
  let cases1: &[(&str, &str, Access, Access, bool)] = &[
    ("", "", N, R, true),
    ("", "userA", N, R, true),
    ("", "userB", N, R, true),
    ("", "userC", N, R, true),
    ("greek", "", N, R, false),
    ("greek", "userA", N, W, true),
    ("greek", "userB", N, W, true),
    ("greek", "userC", N, R, false),
    ("repo", "", N, R, false),
    ("repo", "userA", N, W, true),
    ("repo", "userB", N, R, false),
    ("repo", "userC", N, R, false),
    ("X", "", N, R, false),
    ("X", "userA", N, R, false),
    ("X", "userB", N, R, false),
    ("X", "userC", N, R, false),
  ];
  let authz2 = "[/]\nuserA = r\n\n[/public]\nuserB = rw\n\n[repo:/]\nuserA = rw\n";
  let cases2: &[(&str, &str, Access, Access, bool)] = &[
    ("", "", N, N, true),
    ("", "userA", N, R, true),
    ("", "userB", N, W, true),
    ("", "userC", N, N, true),
    ("greek", "", N, N, false),
    ("greek", "userA", N, R, false),
    ("greek", "userB", N, W, false),
    ("greek", "userC", N, N, false),
  ];
  let authz3 =
    "[/]\nuserA = r\n\n[greek:/public]\nuserB = rw\n\n[repo:/users]\n$authenticated = rw\n";
  let cases3: &[(&str, &str, Access, Access, bool)] = &[
    ("", "", N, N, true),
    ("", "userA", N, R, true),
    ("", "userB", N, N, true),
    ("", "userC", N, N, true),
    ("greek", "", N, N, false),
    ("greek", "userA", N, R, false),
    ("greek", "userB", N, W, true),
    ("greek", "userC", N, N, false),
    ("repo", "", N, N, false),
    ("repo", "userA", N, W, true),
    ("repo", "userB", N, W, true),
    ("repo", "userC", N, W, true),
  ];
  for (contents, cases) in [(authz1, cases1), (authz2, cases2), (authz3, cases3)] {
    let authz = parse(contents);
    for (repository, user, least, most, explicit) in cases {
      let user = (!user.is_empty()).then_some(*user);
      assert_eq!(
        authz.global_rights(repository.as_bytes(), principal(user)),
        GlobalRights {
          every_path: *least,
          some_path: *most,
          explicit: *explicit,
        },
        "{repository:?} {user:?} in {contents:?}"
      );
    }
  }
}

/// `issue_4741_groups`: a group of groups grants each member's users.
#[test]
fn issue_4741_a_group_of_groups_grants_every_member() {
  let authz = parse("[groups]\ng1 = userA\ng2 = userB\ng = @g1, @g2\n\n[/]\n* =\n@g = rw\n");
  check_cases(
    &authz,
    &[
      (Some("/"), Some("repo"), Some("userA"), Need::Write, true),
      (Some("/"), Some("repo"), Some("userB"), Need::Write, true),
    ],
  );
}

/// `reposful_reposless_stanzas_inherit`: `[foo:/]` inherits `[/]` for a user it does not name (issue 4762, 1.14.1).
#[test]
fn issue_4762_a_repository_rule_that_names_another_user_leaves_the_global_rule() {
  let authz = parse(
    "[groups]\ncompany = user1, user2, user3\ncustomer = customer1, customer2\n\n\
# company can read-write on everything\n[/]\n@company = rw\n\n[project1:/]\n@customer = r\n\n[project2:/]\n",
  );
  check_cases(
    &authz,
    &[(
      Some("/foo"),
      Some("project1"),
      Some("user1"),
      Need::WriteBeneath,
      true,
    )],
  );
}

// ---------------------------------------------------------------------------------------------------------------------
// Subversion's own test cases: repos-test.c.

/// `authz`, phase 1: read and write rules, pan-repository rules, inheritance, recursion, questions without a path.
#[test]
fn repos_test_authz_phase_1_every_case() {
  let contents = "[greek:/A]\n* = r\nplato = rw\n\n[greek:/iota]\n* =\n\n[/A/B/lambda]\nplato = r\n* =\n\n\
[greek:/A/D]\nplato = r\n* = r\n\n[greek:/A/D/G]\nplato = r\n* =\n\n[greek:/A/B/E/beta]\n* =\n\n\
[/nowhere]\nnobody = r\n\n";
  check_cases(
    &parse(contents),
    &[
      (Some("/A"), Some("greek"), None, Need::Read, true),
      (Some("/iota"), Some("greek"), None, Need::Read, false),
      (Some("/A"), Some("greek"), Some("plato"), Need::Write, true),
      (Some("/A"), Some("greek"), None, Need::Write, false),
      (
        Some("/A/B/lambda"),
        Some("greek"),
        Some("plato"),
        Need::Read,
        true,
      ),
      (Some("/A/B/lambda"), Some("greek"), None, Need::Read, false),
      (Some("/A/C"), Some("greek"), None, Need::Read, true),
      (
        Some("/A/D"),
        Some("greek"),
        Some("plato"),
        Need::ReadBeneath,
        true,
      ),
      (Some("/A/D"), Some("greek"), None, Need::ReadBeneath, false),
      (None, Some("greek"), Some("plato"), Need::Read, true),
      (None, Some("greek"), None, Need::Write, false),
    ],
  );
}

/// `authz`, phases 2 to 4: a cycle of groups and an undefined group refused, a rule on another directory beneath no
/// part of a recursive question, and a doubled slash in a rule's path refused.
#[test]
fn repos_test_authz_phases_2_to_4() {
  let cycle = refusal(b"[groups]\nslaves = cooks,scribes,@gladiators\ngladiators = equites,thraces,@slaves\n\n[greek:/A]\n@slaves = r\n");
  assert_eq!(cycle, in_rules(2, AuthzRefusal::GroupRecursive));
  assert_eq!(
    cycle.subversion_code(),
    220_003,
    "SVN_ERR_AUTHZ_INVALID_CONFIG"
  );
  assert_eq!(
    refusal(b"[greek:/A]\n@senate = r\n"),
    in_rules(2, AuthzRefusal::EntryGroupUndefined)
  );
  let phase3 = parse("[/]\n* = rw\n\n[greek:/dir2/secret]\n* =\n");
  check_cases(
    &phase3,
    &[(Some("/dir"), Some("greek"), None, Need::ReadBeneath, true)],
  );
  let doubled = refusal(b"[greek:/dir2//secret]\n* =\n");
  assert_eq!(doubled, in_rules(1, AuthzRefusal::RulePathEmptySegment));
  assert_eq!(doubled.subversion_code(), 220_003);
}

/// `test_authz_wildcards`: every wildcard kind, overlapping, at every path of the greek tree and beyond.
#[test]
fn test_authz_wildcards_every_case() {
  let contents = "[:glob:/**/G]\n* = r\n\n[:glob:/A/*/G]\n* =\n\n[:glob:/A/**/*a*]\n* = r\n\n[:glob:/**/*a]\n* = rw\n\n\
[:glob:/A/**/g*]\n* =\n\n[:glob:/**/lambda]\n* = rw\n";
  let anonymous =
    |at: &'static str, need: Need, expected: bool| (Some(at), None, None, need, expected);
  use Need::{Read as R, ReadBeneath as RB, Write as W, WriteBeneath as WB};
  check_cases(
    &parse(contents),
    &[
      anonymous("/", R, false),
      anonymous("/iota", W, true),
      anonymous("/A", R, false),
      anonymous("/A/mu", R, false),
      anonymous("/A/B", R, false),
      anonymous("/A/B/lambda", W, true),
      anonymous("/A/B/E", R, false),
      anonymous("/A/B/E/alpha", W, true),
      anonymous("/A/B/E/beta", W, true),
      anonymous("/A/B/F", R, false),
      anonymous("/A/C", R, false),
      anonymous("/A/D", R, false),
      anonymous("/A/D/gamma", R, false),
      anonymous("/A/D/G", R, false),
      anonymous("/A/D/G/pi", R, false),
      anonymous("/A/D/G/rho", R, false),
      anonymous("/A/D/G/tau", R, true),
      anonymous("/A/D/G/tau", W, false),
      anonymous("/A/D/H", R, false),
      anonymous("/A/D/H/chi", R, false),
      anonymous("/A/D/H/psi", R, false),
      anonymous("/A/D/H/omega", W, true),
      anonymous("/A/G", R, true),
      anonymous("/A/G", W, false),
      anonymous("/A/G/G", R, false),
      anonymous("/G", R, true),
      anonymous("/G", W, false),
      anonymous("/Y/G", R, true),
      anonymous("/Y/G", W, false),
      anonymous("/X/Z/G", R, true),
      anonymous("/X/Z/G", W, false),
      anonymous("/", RB, false),
      anonymous("/iota", RB, true),
      anonymous("/iota", WB, false),
      anonymous("/A", RB, false),
      anonymous("/A/mu", RB, false),
      anonymous("/A/B", RB, false),
      anonymous("/A/B/lambda", RB, false),
      anonymous("/A/B/E", RB, false),
      anonymous("/A/B/E/alpha", RB, false),
      anonymous("/A/B/E/beta", RB, false),
      anonymous("/A/B/F", RB, false),
      anonymous("/A/C", RB, false),
      anonymous("/A/D", RB, false),
      anonymous("/A/D/gamma", RB, false),
      anonymous("/A/D/G", RB, false),
      anonymous("/A/D/G/pi", RB, false),
      anonymous("/A/D/G/rho", RB, false),
      anonymous("/A/D/G/tau", RB, false),
      anonymous("/A/D/H", RB, false),
      anonymous("/A/D/H/chi", RB, false),
      anonymous("/A/D/H/psi", RB, false),
      anonymous("/A/D/H/omega", RB, false),
    ],
  );
}

/// `test_authz_prefixes`: a global and a repository rule for one path, plain or glob, at the root or beneath it, in
/// both orders; the repository's rule decides every time it speaks for the user, whichever comes first.
#[test]
fn test_authz_prefixes_all_sixteen_combinations() {
  let paths = ["/", "/A"];
  for combination in 0..16usize {
    let glob1 = if combination & 1 != 0 { ":glob:" } else { "" };
    let glob2 = if combination & 2 != 0 { ":glob:" } else { "" };
    let repository_first = combination & 4 != 0;
    let (repo1, repo2) = if repository_first {
      ("greek:", "")
    } else {
      ("", "greek:")
    };
    let rule_path = paths[combination / 8];
    let contents = format!(
      "[{glob1}{repo1}{rule_path}]\n* = r\nplato = rw\n\n[{glob2}{repo2}{rule_path}]\n* =\nplato = r\n"
    );
    let authz = parse(&contents);
    for &at in &paths[combination / 8..] {
      let cases: Vec<Case> = if repository_first {
        vec![
          (Some(at), Some("greek"), None, Need::Read, true),
          (Some(at), Some("greek"), None, Need::Write, false),
          (Some(at), Some("greek"), Some("plato"), Need::Read, true),
          (Some(at), Some("greek"), Some("plato"), Need::Write, true),
        ]
      } else {
        vec![
          (Some(at), Some("greek"), None, Need::Read, false),
          (Some(at), Some("greek"), Some("plato"), Need::Read, true),
          (Some(at), Some("greek"), Some("plato"), Need::Write, false),
        ]
      };
      check_cases(&authz, &cases);
    }
  }
}

/// `test_authz_recursive_override`: a later `**` restricts or extends everything beneath it; a later rule for part of
/// a subtree does not.
#[test]
fn test_authz_recursive_override_every_case() {
  let contents = "[:glob:/A/B]\nplato = rw\n\n[:glob:/A/**]\nplato = r\n\n[:glob:/B/C]\nplato =\n\n[:glob:/B/**]\n\
plato = rw\n\n[:glob:/C/D]\nplato = rw\n\n[:glob:/C/**/E]\nplato = r\n\n[:glob:/D/E]\nplato = r\n\n[:glob:/D/**/F]\n\
plato = rw\n";
  let plato =
    |at: &'static str, need: Need, expected: bool| (Some(at), None, Some("plato"), need, expected);
  use Need::{Read as R, ReadBeneath as RB, WriteBeneath as WB};
  check_cases(
    &parse(contents),
    &[
      plato("/", R, false),
      plato("/A", RB, true),
      plato("/A", WB, false),
      plato("/B", RB, true),
      plato("/B", WB, true),
      plato("/C", RB, false),
      plato("/C/D", RB, true),
      plato("/C/D", WB, false),
      plato("/D", RB, false),
      plato("/D/E", RB, true),
      plato("/D/E", WB, false),
    ],
  );
}

/// `test_authz_pattern_tests`, its four files: rules weighed recursively whether or not their paths exist; a global
/// `**` overridden for one path; global patterns beside global path rules; and a `*` that matches one level while a
/// `**` applies to every level.
#[test]
fn test_authz_pattern_tests_every_case() {
  use Need::{Read as R, ReadBeneath as RB, Write as W, WriteBeneath as WB};
  let contents1 = "[:glob:/**/Yeti]\nplato = r\n\n[/]\nplato = r\n\n[/trunk]\nplato = rw\n";
  let plato =
    |at: &'static str, need: Need, expected: bool| (Some(at), None, Some("plato"), need, expected);
  check_cases(
    &parse(contents1),
    &[
      plato("/", RB, true),
      plato("/", WB, false),
      plato("/trunk", RB, true),
      plato("/trunk", WB, false),
      plato("/trunk", W, true),
      plato("/trunk/A/B/C", W, true),
      plato("/trunk/A/B/C/Yeti", W, false),
      plato("/trunk/Yeti", W, false),
      plato("/Yeti", W, false),
    ],
  );
  let contents2 =
    "[:glob:/X]\nuser1 =\n\n[:glob:/X/**]\nuser1 = rw\nuser2 = rw\n\n[:glob:/X/Y/Z]\nuser2 =\n";
  let as_user = |at: &'static str, user: &'static str, need: Need, expected: bool| {
    (Some(at), None, Some(user), need, expected)
  };
  check_cases(
    &parse(contents2),
    &[
      as_user("/", "user1", R, false),
      as_user("/", "user2", R, false),
      as_user("/X", "user1", WB, true),
      as_user("/X/Y", "user1", RB, true),
      as_user("/X/Y/Z", "user1", RB, true),
      as_user("/X", "user1", RB, true),
      as_user("/X", "user2", WB, false),
      as_user("/X/Y", "user2", WB, false),
      as_user("/X/Y/Z", "user2", WB, false),
      as_user("/X", "user2", W, true),
      as_user("/X/A", "user2", WB, true),
      as_user("/X/Y/A", "user2", WB, true),
    ],
  );
  let contents3 = "[groups]\nTeam1 = user1\nTeam2 = user1, user2\n\n[/]\n* =\n\n[:glob:Repo1:/**/folder*]\n\
@Team1 = rw\n\n[Repo2:/]\n@Team2 = r\n";
  let in_repo =
    |at: &'static str, repository: &'static str, user: &'static str, need: Need, expected: bool| {
      (Some(at), Some(repository), Some(user), need, expected)
    };
  check_cases(
    &parse(contents3),
    &[
      in_repo("/", "Repo1", "user1", R, false),
      in_repo("/", "Repo1", "user2", R, false),
      in_repo("/", "Repo2", "user1", R, true),
      in_repo("/", "Repo2", "user2", R, true),
      in_repo("/", "Repo2", "user1", W, false),
      in_repo("/", "Repo2", "user2", W, false),
      in_repo("/folder_1", "Repo1", "user1", WB, true),
      in_repo("/folder_1", "Repo1", "user2", R, false),
      in_repo("/1_folder", "Repo1", "user1", R, false),
      in_repo("/foo/bar/folder_2/random", "Repo1", "user1", WB, true),
      in_repo("/foo/bar/folder_2/random", "Repo1", "user2", R, false),
      in_repo("/foo/bar/2_folder/random", "Repo1", "user1", R, false),
      in_repo("/foo/bar/folder", "Repo1", "user1", WB, true),
      in_repo("/foo/bar/folder", "Repo1", "user2", R, false),
      in_repo("/foo/bar/folde", "Repo1", "user1", R, false),
      in_repo("/foo/bar/folde", "Repo1", "user2", R, false),
    ],
  );
  let contents4 = "[groups]\nteam1 = user1, user3\nteam2 = user2, user3\n\n[:glob:Repo1:/trunk/**]\n@team2 = rw\n\n\
[:glob:Repo1:/trunk/*]\n@team1 = r\n";
  check_cases(
    &parse(contents4),
    &[
      in_repo("/trunk", "Repo1", "user1", R, false),
      in_repo("/trunk", "Repo1", "user2", W, true),
      in_repo("/trunk", "Repo1", "user3", W, true),
      in_repo("/trunk/A", "Repo1", "user1", R, true),
      in_repo("/trunk/A", "Repo1", "user3", R, true),
      in_repo("/trunk/A", "Repo1", "user1", W, false),
      in_repo("/trunk/A", "Repo1", "user2", W, true),
      in_repo("/trunk/A", "Repo1", "user3", W, false),
      in_repo("/trunk/A/B", "Repo1", "user2", WB, true),
      in_repo("/trunk/A/B", "Repo1", "user3", WB, true),
      in_repo("/trunk/A/B", "Repo1", "user1", R, true),
      in_repo("/trunk/A/B", "Repo1", "user1", W, false),
    ],
  );
}

/// `groups_authz`: groups in a global groups file; rules in it refused; groups in both files refused; a cycle in it
/// refused.
#[test]
fn groups_authz_every_case() {
  let groups = "[groups]\nslaves = pamphilos,@gladiators\ngladiators = demetrius,galenos\nphilosophers = plato\n\n";
  let rules = "[greek:/A]\n@slaves = \n@gladiators = r\n@philosophers = rw\n\n";
  let at_a = |user: Option<&'static str>, need: Need, expected: bool| {
    (Some("/A"), Some("greek"), user, need, expected)
  };
  check_cases(
    &parse_with_groups(rules, groups),
    &[
      at_a(None, Need::Read, false),
      at_a(Some("plato"), Need::Read, true),
      at_a(Some("demetrius"), Need::Read, true),
      at_a(Some("galenos"), Need::Read, true),
      at_a(Some("pamphilos"), Need::Read, false),
      at_a(None, Need::Write, false),
      at_a(Some("plato"), Need::Write, true),
      at_a(Some("demetrius"), Need::Write, false),
      at_a(Some("galenos"), Need::Write, false),
      at_a(Some("pamphilos"), Need::Write, false),
    ],
  );
  let rules_b = "[greek:/B]\n@philosophers = rw\n\n";
  assert_eq!(
    refusal_with_groups(
      rules_b.as_bytes(),
      b"[groups]\nphilosophers = socrates\n\n[greek:/A]\n@philosophers = rw\n\n"
    ),
    in_groups(4, AuthzRefusal::SectionInvalid)
  );
  let in_b = |at: &'static str, user: Option<&'static str>, need: Need, expected: bool| {
    (Some(at), Some("greek"), user, need, expected)
  };
  check_cases(
    &parse_with_groups(rules_b, "[groups]\nphilosophers = socrates\n\n"),
    &[
      in_b("/A", None, Need::Read, false),
      in_b("/A", Some("socrates"), Need::Read, false),
      in_b("/B", None, Need::Read, false),
      in_b("/B", Some("socrates"), Need::Read, true),
      in_b("/A", None, Need::Write, false),
      in_b("/A", Some("socrates"), Need::Write, false),
      in_b("/B", None, Need::Write, false),
      in_b("/B", Some("socrates"), Need::Write, true),
    ],
  );
  assert_eq!(
    refusal_with_groups(
      b"[greek:/A]\n@slaves = \n@kings = rw\n\n[groups]\nkings = maximus\n\n",
      b"[groups]\nslaves = maximus\n\n"
    ),
    in_rules(6, AuthzRefusal::GroupsWithGroupsFile)
  );
  assert_eq!(
    refusal_with_groups(
      b"[greek:/A]\n@slaves = r\n\n",
      b"[groups]\nslaves = cooks,scribes,@gladiators\ngladiators = equites,thraces,@slaves\n\n"
    ),
    in_groups(2, AuthzRefusal::GroupRecursive)
  );
}

/// `in_repo_authz` and `in_repo_groups_authz`: files that open with blank lines and end without a line feed.
#[test]
fn in_repo_authz_and_in_repo_groups_authz_every_case() {
  let at_a = |user: Option<&'static str>, need: Need, expected: bool| {
    (Some("/A"), None, user, need, expected)
  };
  check_cases(
    &parse("\n\n[/]\nplato = rw\nsocrates = r"),
    &[
      at_a(None, Need::Read, false),
      at_a(Some("plato"), Need::Read, true),
      at_a(Some("socrates"), Need::Read, true),
      at_a(None, Need::Write, false),
      at_a(Some("socrates"), Need::Write, false),
      at_a(Some("plato"), Need::Write, true),
    ],
  );
  check_cases(
    &parse_with_groups(
      "[/]\n@senate = rw\n@philosophers = r\n\n",
      "[groups]\nphilosophers = plato, socrates\nsenate = solon, ephialtes\n\n",
    ),
    &[
      at_a(None, Need::Read, false),
      at_a(Some("plato"), Need::Read, true),
      at_a(Some("socrates"), Need::Read, true),
      at_a(Some("solon"), Need::Read, true),
      at_a(Some("ephialtes"), Need::Read, true),
      at_a(None, Need::Write, false),
      at_a(Some("plato"), Need::Write, false),
      at_a(Some("socrates"), Need::Write, false),
      at_a(Some("solon"), Need::Write, true),
      at_a(Some("ephialtes"), Need::Write, true),
    ],
  );
}

/// `issue_4060`: a deeper rule granting write beneath a root that grants read, with the repository named, empty and
/// absent.
#[test]
fn issue_4060_recursive_write_beneath_a_deeper_rule() {
  let authz = parse("[/A/B]\nozymandias = rw\n[/]\nozymandias = r\n\n");
  for repository in [Some("babylon"), Some(""), None] {
    check_cases(
      &authz,
      &[(
        Some("/A/B/C"),
        repository,
        Some("ozymandias"),
        Need::WriteBeneath,
        true,
      )],
    );
  }
}

/// `commit_editor_authz`: each editor action of the test, as the questions §4.17.4 item 10 has a commit ask (write on
/// the path and its parent; recursive write to delete a directory; recursive read of a copy source), granted exactly
/// when Subversion's editor let the action through.
#[test]
fn commit_editor_authz_every_action() {
  let authz = parse(
    "\n\n[/]\nplato = r\n\n[/A]\nplato = rw\n\n[/A/alpha]\nplato = \n\n[/A/C]\n\nplato = \n\n[/A/D]\nplato = rw\n\n\
[/A/D/G]\nplato = r",
  );
  let rules = authz.rules(b"", principal(Some("plato")));
  let writes = |at: &str| rules.access(path(at)) >= Access::Write;
  let writes_beneath = |at: &str| rules.access_beneath(path(at)) >= Access::Write;
  let reads_beneath = |at: &str| rules.access_beneath(path(at)) >= Access::Read;
  // (action, allowed by the questions, Subversion's editor refused it)
  let actions: &[(&str, bool, bool)] = &[
    ("delete /iota", writes("/iota") && writes("/"), true),
    ("change a property of /iota", writes("/iota"), true),
    ("add /alpha", writes("/alpha") && writes("/"), true),
    (
      "add /alpha from /A/B/lambda",
      writes("/alpha") && writes("/") && reads_beneath("/A/B/lambda"),
      true,
    ),
    ("add directory /I", writes("/I") && writes("/"), true),
    (
      "add directory /J from /A/D",
      writes("/J") && writes("/") && reads_beneath("/A/D"),
      true,
    ),
    ("add /A/alpha", writes("/A/alpha") && writes("/A"), true),
    (
      "add /A/B/theta",
      writes("/A/B/theta") && writes("/A/B"),
      false,
    ),
    ("delete /A/mu", writes("/A/mu") && writes("/A"), false),
    ("add directory /A/E", writes("/A/E") && writes("/A"), false),
    (
      "add directory /A/J from /A/D",
      writes("/A/J") && writes("/A") && reads_beneath("/A/D"),
      false,
    ),
    (
      "delete directory /A/D/G",
      writes_beneath("/A/D/G") && writes("/A/D"),
      true,
    ),
    (
      "delete directory /A/D/H",
      writes_beneath("/A/D/H") && writes("/A/D"),
      false,
    ),
    (
      "change a property of /A/D/gamma",
      writes("/A/D/gamma"),
      false,
    ),
  ];
  for (action, allowed, refused) in actions {
    assert_eq!(*allowed, !refused, "{action}");
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// APR's apr_fnmatch table (test/testfnmatch.c), at flags 0, and apr_fnmatch_test's.

/// Every row of `testfnmatch.c`'s table that flags 0 decides: `SUCCEEDS` and `FAILS_IF(flag)` rows match,
/// `FAILS`, `SUCCEEDS_IF(flag)` and rows that require a flag do not.
#[test]
fn apr_fnmatch_answers_aprs_own_table_at_flags_zero() {
  let rows: &[(&str, &str, bool)] = &[
    ("", "test", false),
    ("", "*", false),
    ("test", "*", false),
    ("test", "test", true),
    ("te\\st", "test", true),
    ("te\\\\st", "te\\st", true),
    ("te\\*t", "te*t", true),
    ("te\\*t", "test", false),
    ("te\\?t", "te?t", true),
    ("te\\?t", "test", false),
    ("tesT", "test", false),
    ("test", "Test", false),
    ("tEst", "teSt", false),
    ("?est", "test", true),
    ("te?t", "test", true),
    ("tes?", "test", true),
    ("test?", "test", false),
    ("*", "", true),
    ("*", "test", true),
    ("*test", "test", true),
    ("*est", "test", true),
    ("*st", "test", true),
    ("t*t", "test", true),
    ("te*t", "test", true),
    ("te*st", "test", true),
    ("te*", "test", true),
    ("tes*", "test", true),
    ("test*", "test", true),
    (".[\\-\\t]", ".t", true),
    ("test*?*[a-z]*", "testgoop", true),
    ("te[^x]t", "test", true),
    ("te[^abc]t", "test", true),
    ("te[^x]t", "test", true),
    ("te[!x]t", "test", true),
    ("te[^x]t", "text", false),
    ("te[^\\x]t", "text", false),
    ("te[^x\\", "text", false),
    ("te[/]t", "text", false),
    ("te[S]t", "test", false),
    ("te[r-t]t", "test", true),
    ("te[r-t]t", "teSt", false),
    ("te[r-T]t", "test", false),
    ("te[R-T]t", "test", false),
    ("te[r-Tz]t", "tezt", true),
    ("te[R-T]t", "tent", false),
    ("tes[]t]", "test", true),
    ("tes[t-]", "test", true),
    ("tes[t-]]", "test]", true),
    ("tes[t-]]", "test", false),
    ("tes[u-]", "test", false),
    ("tes[t-]", "tes[t-]", false),
    ("test[/-/]", "test[/-/]", false),
    ("test[\\/-/]", "test[/-/]", false),
    ("test[/-\\/]", "test[/-/]", false),
    ("test[/-/]", "test/", true),
    ("test[\\/-/]", "test/", true),
    ("test[/-\\/]", "test/", true),
    ("/", "", false),
    ("", "/", false),
    ("/test", "test", false),
    ("test", "/test", false),
    ("test/", "test", false),
    ("test", "test/", false),
    ("\\/test", "/test", true),
    ("*test", "/test", true),
    ("/*/test/", "/test", false),
    ("/*/test/", "/test/test/", true),
    ("test/this", "test/", false),
    ("test/", "test/this", false),
    ("test*/this", "test/this", true),
    ("test*/this", "test/that", false),
    ("test/*this", "test/this", true),
    (".*", ".this", true),
    ("*", ".this", true),
    ("?this", ".this", true),
    ("[.]this", ".this", true),
    ("test/this", "test/this", true),
    ("test?this", "test/this", true),
    ("test*this", "test/this", true),
    ("test[/]this", "test/this", true),
    ("test/.*", "test/.this", true),
    ("test/*", "test/.this", true),
    ("test/?this", "test/.this", true),
    ("test/[.]this", "test/.this", true),
  ];
  for (pattern, string, expected) in rows {
    assert_eq!(
      glob::fnmatch(pattern.as_bytes(), string.as_bytes()),
      *expected,
      "apr_fnmatch({pattern:?}, {string:?}, 0)"
    );
  }
}

/// `test_fnmatch_test`: which patterns `apr_fnmatch_test` finds wildcards in.
#[test]
fn apr_fnmatch_test_answers_aprs_own_table() {
  for (pattern, expected) in [
    ("a*b", true),
    ("a?", true),
    ("a\\b?", true),
    ("a[b-c]", true),
    ("a", false),
    ("a\\", false),
  ] {
    assert_eq!(
      glob::has_wildcards(pattern.as_bytes()),
      expected,
      "{pattern:?}"
    );
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// The documentation's files and stated outcomes.

/// The `conf/authz` `svnadmin create` writes (`libsvn_repos/repos.c` 921-951), as written: everything commented out
/// but two empty sections, so it parses and nobody may do anything.
#[test]
fn the_conf_authz_svnadmin_create_writes_parses_and_grants_nothing() {
  let template = "### This file is an example authorization file for svnserve.\n### Its format is identical to that \
of mod_authz_svn authorization\n### files.\n### As shown below each section defines authorizations for the path \
and\n### (optional) repository specified by the section name.\n### The authorizations follow. An authorization \
line can refer to:\n###  - a single user,\n###  - a group of users defined in a special [groups] section,\n###  - \
an alias defined in a special [aliases] section,\n###  - all authenticated users, using the '$authenticated' \
token,\n###  - only anonymous users, using the '$anonymous' token,\n###  - anyone, using the '*' wildcard.\n###\n\
### A match can be inverted by prefixing the rule with '~'. Rules can\n### grant read ('r') access, read-write \
('rw') access, or no access\n### ('').\n\n[aliases]\n# joe = /C=XZ/ST=Dessert/L=Snake City/O=Snake Oil, \
Ltd./OU=Research Institute/CN=Joe Average\n\n[groups]\n# harry_and_sally = harry,sally\n# harry_sally_and_joe = \
harry,sally,&joe\n\n# [/foo/bar]\n# harry = rw\n# &joe = r\n# * =\n\n# [repository:/baz/fuz]\n# \
@harry_and_sally = rw\n# * = r\n";
  let authz = parse(template);
  for user in [None, Some("harry")] {
    let rules = authz.rules(b"repository", principal(user));
    assert_eq!(rules.access(path("/foo/bar")), Access::None);
    assert_eq!(rules.access_anywhere(), Access::None);
  }
}

/// The same template with its examples uncommented: an alias whose user holds `=` and `,`, a group naming it, and the
/// two example rules.
#[test]
fn the_conf_authz_examples_uncommented_mean_what_they_say() {
  let joe = "/C=XZ/ST=Dessert/L=Snake City/O=Snake Oil, Ltd./OU=Research Institute/CN=Joe Average";
  let authz = parse(&format!(
    "[aliases]\njoe = {joe}\n\n[groups]\nharry_and_sally = harry,sally\nharry_sally_and_joe = harry,sally,&joe\n\n\
[/foo/bar]\nharry = rw\n&joe = r\n* =\n\n[repository:/baz/fuz]\n@harry_and_sally = rw\n* = r\n"
  ));
  let at = |repository: &str, user: Option<&str>, at: &str| {
    authz
      .rules(repository.as_bytes(), principal(user))
      .access(path(at))
  };
  assert_eq!(at("any", Some("harry"), "/foo/bar"), Access::Write);
  assert_eq!(at("any", Some(joe), "/foo/bar/x"), Access::Read);
  assert_eq!(at("any", Some("sally"), "/foo/bar"), Access::None);
  assert_eq!(at("any", None, "/foo/bar"), Access::None);
  assert_eq!(at("repository", Some("sally"), "/baz/fuz"), Access::Write);
  assert_eq!(at("repository", Some(joe), "/baz/fuz"), Access::Read);
  assert_eq!(at("repository", None, "/baz/fuz/deeper"), Access::Read);
  assert_eq!(
    at("other", None, "/baz/fuz"),
    Access::None,
    "the rule names its repository"
  );
}

/// The book's examples (path-based authorization, svnbook 1.7), each with the outcome its prose states.
#[test]
fn the_books_examples_have_the_outcomes_the_book_states() {
  // "harry has full read and write access ... sally has read-only access. Any other users are blocked".
  let first = parse("[calc:/branches/calc/bug-142]\nharry = rw\nsally = r\n");
  let at = |authz: &Authz, user: &str, at: &str| {
    authz.rules(b"calc", principal(Some(user))).access(path(at))
  };
  assert_eq!(at(&first, "harry", "/branches/calc/bug-142"), Access::Write);
  assert_eq!(
    at(&first, "sally", "/branches/calc/bug-142/src"),
    Access::Read
  );
  assert_eq!(at(&first, "frank", "/branches/calc/bug-142"), Access::None);
  // "Now Sally can write to the testing subdirectory of the branch, but can still only read other parts."
  let second = parse(
    "[calc:/branches/calc/bug-142]\nharry = rw\nsally = r\n\n# give sally write access only to the 'testing' subdir\n\
[calc:/branches/calc/bug-142/testing]\nsally = rw\n",
  );
  assert_eq!(
    at(&second, "sally", "/branches/calc/bug-142/testing/x"),
    Access::Write
  );
  assert_eq!(
    at(&second, "sally", "/branches/calc/bug-142/other"),
    Access::Read
  );
  assert_eq!(
    at(&second, "harry", "/branches/calc/bug-142/testing"),
    Access::Write
  );
  // "Harry has read/write access to the entire bug-142 tree, but has absolutely no access at all to the secret
  // subdirectory".
  let third = parse(
    "[calc:/branches/calc/bug-142]\nharry = rw\nsally = r\n\n[calc:/branches/calc/bug-142/secret]\nharry =\n",
  );
  assert_eq!(
    at(&third, "harry", "/branches/calc/bug-142/secret"),
    Access::None
  );
  assert_eq!(
    at(&third, "harry", "/branches/calc/bug-142/public"),
    Access::Write
  );
  // "This makes all repositories world-readable to all users."
  let fourth = parse("[/]\n* = r\n");
  for repository in [&b"calc"[..], b"paint", b""] {
    for user in [None, Some("anyone")] {
      assert_eq!(
        authz_access(&fourth, repository, user, "/deep/path"),
        Access::Read
      );
    }
  }
}

/// What `user` may do at `at` in `repository`.
fn authz_access(authz: &Authz, repository: &[u8], user: Option<&str>, at: &str) -> Access {
  authz.rules(repository, principal(user)).access(path(at))
}

/// The book's groups and aliases: "the combination of all matching permissions is granted", groups of groups, and
/// aliases standing for long names wherever a name may stand.
#[test]
fn the_books_groups_and_aliases_combine_as_the_book_states() {
  let groups = "[groups]\ncalc-developers = harry, sally, joe\npaint-developers = frank, sally, jane\n\
everyone = harry, sally, joe, frank, jane\n";
  let authz = parse(&format!(
    "{groups}\n[calc:/projects/calc]\n@calc-developers = rw\n\n[paint:/projects/paint]\njane = r\n@paint-developers = rw\n"
  ));
  assert_eq!(
    authz_access(&authz, b"paint", Some("jane"), "/projects/paint"),
    Access::Write,
    "jane = r and the group's rw"
  );
  assert_eq!(
    authz_access(&authz, b"calc", Some("sally"), "/projects/calc"),
    Access::Write
  );
  assert_eq!(
    authz_access(&authz, b"calc", Some("frank"), "/projects/calc"),
    Access::None
  );
  // A group of groups (its rule is this test's: the book defines the group alone).
  let nested = parse(
    "[groups]\ncalc-developers = harry, sally, joe\npaint-developers = frank, sally, jane\n\
everyone = @calc-developers, @paint-developers\n\n[/]\n@everyone = r\n",
  );
  for user in ["harry", "jane", "frank"] {
    assert_eq!(
      authz_access(&nested, b"calc", Some(user), "/"),
      Access::Read,
      "{user}"
    );
  }
  assert_eq!(
    authz_access(&nested, b"calc", Some("mallory"), "/"),
    Access::None
  );
  // Aliases (the book elides frank's and jane's; they are this test's).
  let harry = "CN=Harold Hacker,OU=Engineers,DC=red-bean,DC=com";
  let aliased = parse(&format!(
    "[aliases]\nharry = {harry}\nsally = CN=Sally Swatterbug,OU=Engineers,DC=red-bean,DC=com\n\
joe = CN=Gerald I. Joseph,OU=Engineers,DC=red-bean,DC=com\nfrank = CN=Frank,OU=Art\njane = CN=Jane,OU=Art\n\n\
[groups]\ncalc-developers = &harry, &sally, &joe\npaint-developers = &frank, &sally, &jane\n\
everyone = @calc-developers, @paint-developers\n\n[calc:/projects/calc]\n@calc-developers = rw\n"
  ));
  assert_eq!(
    authz_access(&aliased, b"calc", Some(harry), "/projects/calc"),
    Access::Write
  );
  assert_eq!(
    authz_access(&aliased, b"calc", Some("harry"), "/projects/calc"),
    Access::None,
    "an alias is not a user"
  );
}

/// The book's authentication tokens, and its inverted block "equivalent to the one in the previous example".
#[test]
fn the_books_tokens_and_their_inverted_equivalent_agree() {
  let plain = parse("[calendar:/projects/calendar]\n$anonymous = r\n$authenticated = rw\n");
  let inverted = parse("[calendar:/projects/calendar]\n~$authenticated = r\n~$anonymous = rw\n");
  for authz in [&plain, &inverted] {
    assert_eq!(
      authz_access(authz, b"calendar", None, "/projects/calendar"),
      Access::Read
    );
    assert_eq!(
      authz_access(authz, b"calendar", Some("harry"), "/projects/calendar/x"),
      Access::Write
    );
  }
}

/// The book's exclusion example: "only allow the owners to make and modify release tags" (the aliases the book leaves
/// out are this test's).
#[test]
fn the_books_exclusion_lets_only_the_owners_write_tags() {
  let authz = parse(
    "[aliases]\nharry = harry\nsally = sally\njoe = joe\nhewlett = hewlett\npackard = packard\n\n\
[groups]\ncalc-developers = &harry, &sally, &joe\ncalc-owners = &hewlett, &packard\ncalc = @calc-developers, @calc-owners\n\n\
# Any calc participant has read-write access...\n[calc:/projects/calc]\n@calc = rw\n\n\
# ...but only allow the owners to make and modify release tags.\n[calc:/projects/calc/tags]\n~@calc-owners = r\n",
  );
  assert_eq!(
    authz_access(&authz, b"calc", Some("harry"), "/projects/calc/trunk"),
    Access::Write
  );
  assert_eq!(
    authz_access(&authz, b"calc", Some("harry"), "/projects/calc/tags/1.0"),
    Access::Read
  );
  assert_eq!(
    authz_access(&authz, b"calc", Some("hewlett"), "/projects/calc/tags/1.0"),
    Access::Write
  );
  assert_eq!(
    authz_access(&authz, b"calc", None, "/projects/calc/tags"),
    Access::None
  );
}

/// The book's rule on a file.
#[test]
fn the_books_rule_on_a_file() {
  let authz = parse("[calendar:/projects/calendar/manager.ics]\nharry = rw\nsally = r\n");
  assert_eq!(
    authz_access(
      &authz,
      b"calendar",
      Some("harry"),
      "/projects/calendar/manager.ics"
    ),
    Access::Write
  );
  assert_eq!(
    authz_access(
      &authz,
      b"calendar",
      Some("sally"),
      "/projects/calendar/manager.ics"
    ),
    Access::Read
  );
  assert_eq!(
    authz_access(
      &authz,
      b"calendar",
      Some("sally"),
      "/projects/calendar/other.ics"
    ),
    Access::None
  );
}

/// The 1.10 release notes: two sections for one path refused ("in 1.10 this raises an error"), and repeated entries
/// for one name, alias or group combined ("1.10 combines all the lines to give read-write access").
#[test]
fn the_1_10_notes_refuse_a_repeated_section_and_combine_repeated_entries() {
  assert_eq!(
    refusal(b"[/some/path]\nuserA = r\n[/some/path]\nuserB = rw\n"),
    in_rules(3, AuthzRefusal::SectionRepeated)
  );
  let authz = parse(
    "[aliases]\nalias = aliased\n[groups]\ngroup = grouped\n\
[/some/path]\nuser = rw\nuser = r\n&alias = rw\n&alias = r\n@group = rw\n@group = r\n",
  );
  for user in ["user", "aliased", "grouped"] {
    assert_eq!(
      authz_access(&authz, b"", Some(user), "/some/path"),
      Access::Write,
      "{user}"
    );
  }
}

/// The 1.10 release notes on wildcards: `*` one segment, `**` any number, `/*/**/*`, `/**/*/*` and `/*/*/**` one rule,
/// a plain and a glob section for one path one rule, classic patterns with escapes, and write alone refused.
#[test]
fn the_1_10_notes_on_wildcards() {
  let one = parse("[:glob:/*/x]\n* = r\n");
  assert_eq!(authz_access(&one, b"", None, "/a/x"), Access::Read);
  assert_eq!(
    authz_access(&one, b"", None, "/a/b/x"),
    Access::None,
    "* is exactly one segment"
  );
  let many = parse("[:glob:/a/**/x]\n* = r\n");
  for at in ["/a/x", "/a/b/x", "/a/b/c/x"] {
    assert_eq!(authz_access(&many, b"", None, at), Access::Read, "{at}");
  }
  for (first, second) in [
    ("/*/**/*", "/**/*/*"),
    ("/*/**/*", "/*/*/**"),
    ("/**/*/*", "/*/*/**"),
  ] {
    assert_eq!(
      refusal(format!("[:glob:{first}]\n* = r\n[:glob:{second}]\n* = r\n").as_bytes()),
      in_rules(3, AuthzRefusal::RuleRepeated),
      "{first} and {second}"
    );
  }
  assert_eq!(
    refusal(b"[/path/without/wildcards]\n* = r\n[:glob:/path/without/wildcards]\n* = r\n"),
    in_rules(3, AuthzRefusal::RuleRepeated)
  );
  let classic = parse("[:glob:/*foo*.bar]\n* = r\n[:glob:/x\\*y]\n* = rw\n");
  assert_eq!(
    authz_access(&classic, b"", None, "/afoob.bar"),
    Access::Read
  );
  assert_eq!(
    authz_access(&classic, b"", None, "/afoob.baz"),
    Access::None
  );
  assert_eq!(
    authz_access(&classic, b"", None, "/x*y"),
    Access::Write,
    "an escaped * is a literal"
  );
  assert_eq!(authz_access(&classic, b"", None, "/xzy"), Access::None);
  assert_eq!(
    refusal(b"[/]\n* = w\n"),
    in_rules(2, AuthzRefusal::AccessWriteOnly)
  );
}

/// The 1.14 release notes' example of issue 4762: "developer1 has rw access to any path in project1", the global rule
/// applying since the repository's does not match developer1.
#[test]
fn the_1_14_notes_issue_4762_example() {
  let authz = parse(
    "[groups]\ncompany = developer1, developer2, developer3\ncustomer = customer1, customer2\n\n\
# company can read-write on everything\n[/]\n@company = rw\n\n[project1:/]\n@customer = r\n",
  );
  assert_eq!(
    authz_access(&authz, b"project1", Some("developer1"), "/trunk"),
    Access::Write
  );
  assert_eq!(
    authz_access(&authz, b"project1", Some("customer1"), "/trunk"),
    Access::Read
  );
  assert_eq!(
    authz_access(&authz, b"project2", Some("customer1"), "/trunk"),
    Access::None
  );
}

// ---------------------------------------------------------------------------------------------------------------------
// The file's lines, as svn_config__parse_stream reads them.

/// A byte order mark, CR LF line ends, `:` separators, a header's trailing text, comments between entries, and a value
/// continued onto the next lines all read as Subversion reads them.
#[test]
fn the_lines_read_as_subversions_config_reader_reads_them() {
  let authz = parse(
    "\u{feff}[/] trailing text after the header is ignored\r\n# a comment\r\nharry: r\r\n  w\r\nsally = \r\n\tr\r\n\r\n",
  );
  assert_eq!(
    authz_access(&authz, b"", Some("harry"), "/"),
    Access::Write,
    "r, continued by w"
  );
  assert_eq!(
    authz_access(&authz, b"", Some("sally"), "/"),
    Access::Read,
    "nothing, continued by r"
  );
  assert_eq!(
    refusal(b"[/x]\nlong = r\n  # a continuation line, not a comment\n"),
    in_rules(2, AuthzRefusal::AccessModeInvalid),
    "an indented # continues the value above it"
  );
}

/// Names are compared byte for byte: case counts, and whitespace inside a name is part of it.
#[test]
fn names_are_bytes_case_and_spaces_included() {
  let authz = parse("[/]\nHarry = rw\nharry potter = r\n");
  assert_eq!(authz_access(&authz, b"", Some("Harry"), "/"), Access::Write);
  assert_eq!(authz_access(&authz, b"", Some("harry"), "/"), Access::None);
  assert_eq!(
    authz_access(&authz, b"", Some("harry potter"), "/"),
    Access::Read
  );
  let colon_first = refusal(b"[/]\na:b = r\n");
  assert_eq!(
    colon_first,
    in_rules(2, AuthzRefusal::AccessModeInvalid),
    "a ':' before '=' separates: value 'b = r'"
  );
}

/// A bracket expression cannot be written in a section header: the header ends at its first `]`
/// (`parse_section_name`), so `[:glob:/[ab]]` is the glob section `/[ab`, a literal, and the `]` after it is ignored.
#[test]
fn a_section_header_ends_at_its_first_closing_bracket() {
  let authz = parse("[:glob:/[ab]]\n* = r\n");
  assert_eq!(authz_access(&authz, b"", None, "/[ab"), Access::Read);
  assert_eq!(authz_access(&authz, b"", None, "/a"), Access::None);
}

/// A carriage return that opens a line or follows `[` is dropped, as `parser_getc` drops it.
#[test]
fn carriage_returns_at_a_lines_start_and_after_the_bracket_are_dropped() {
  let authz = parse("\r[\r/a]\n\rharry = r\n");
  assert_eq!(authz_access(&authz, b"", Some("harry"), "/a"), Access::Read);
}

/// A blank line ends a value; an indented line after it is no continuation.
#[test]
fn a_blank_line_ends_a_value() {
  assert_eq!(
    refusal(b"[/]\nharry = r\n\n  w\n"),
    in_rules(4, AuthzRefusal::OptionExpected)
  );
}

// ---------------------------------------------------------------------------------------------------------------------
// Every refusal by kind, with its line and its Subversion error.

/// Each lexical refusal of `svn_config__parse_stream`, at its line, answered `SVN_ERR_MALFORMED_FILE`.
#[test]
fn every_lexical_refusal_by_kind_and_line() {
  let cases: &[(&[u8], usize, AuthzRefusal)] = &[
    (b"[/]\n [/x]\n", 2, AuthzRefusal::SectionHeaderIndented),
    (b"[/]\n  # x\n", 2, AuthzRefusal::CommentIndented),
    (b"# x\nharry = r\n", 2, AuthzRefusal::SectionHeaderExpected),
    (b"[/]\n  harry = r\n", 2, AuthzRefusal::OptionExpected),
    (b"[/]\nharry r\n", 2, AuthzRefusal::OptionSeparatorMissing),
    (b"[/]\n= r\n", 2, AuthzRefusal::OptionNameEmpty),
    (b"[/]\n: r\n", 2, AuthzRefusal::OptionNameEmpty),
    (b"\n\n[/x\n", 3, AuthzRefusal::SectionHeaderUnterminated),
    (b"[/]\nharry = r\x00\n", 2, AuthzRefusal::NulByte),
  ];
  for (bytes, line, kind) in cases {
    let refused = refusal(bytes);
    assert_eq!(
      refused,
      in_rules(*line, *kind),
      "{:?}",
      String::from_utf8_lossy(bytes)
    );
    assert_eq!(refused.subversion_code(), 200_002, "SVN_ERR_MALFORMED_FILE");
  }
}

/// Each refusal of the authz constructor, at its line, answered `SVN_ERR_AUTHZ_INVALID_CONFIG`.
#[test]
fn every_section_and_entry_refusal_by_kind_and_line() {
  let cases: &[(&[u8], usize, AuthzRefusal)] = &[
    (b"[/a]\n[/b]\n[/a]\n", 3, AuthzRefusal::SectionRepeated),
    (b"[groups]\n[groups]\n", 2, AuthzRefusal::SectionRepeated),
    (b"[aliases]\n[aliases]\n", 2, AuthzRefusal::SectionRepeated),
    (b"[foo]\n", 1, AuthzRefusal::SectionInvalid),
    (b"[repo:groups]\n", 1, AuthzRefusal::SectionInvalid),
    (b"[repo:a/b]\n", 1, AuthzRefusal::SectionInvalid),
    (b"[ /a]\n", 1, AuthzRefusal::SectionInvalid),
    (b"[]\n", 1, AuthzRefusal::SectionInvalid),
    (b"[:glob:groups]\n", 1, AuthzRefusal::SectionInvalid),
    (b"[:/a]\n", 1, AuthzRefusal::RepositoryNameEmpty),
    (b"[:glob]\n", 1, AuthzRefusal::RepositoryNameEmpty),
    (b"[:glob::/a]\n", 1, AuthzRefusal::RepositoryNameEmpty),
    (b"[:a]\n", 1, AuthzRefusal::RepositoryNameEmpty),
    (b"[:xyz:/a]\n", 1, AuthzRefusal::RuleTypeInvalid),
    (b"[::/a]\n", 1, AuthzRefusal::RuleTypeInvalid),
    (b"[/a//b]\n", 1, AuthzRefusal::RulePathEmptySegment),
    (b"[/a/]\n", 1, AuthzRefusal::RulePathEmptySegment),
    (b"[:glob:/a/./b]\n", 1, AuthzRefusal::RulePathDot),
    (b"[r:/a/..]\n", 1, AuthzRefusal::RulePathDotDot),
    (b"[/a]\n[:glob:/a]\n", 2, AuthzRefusal::RuleRepeated),
    (b"[r:/a/b]\n[:glob:r:/a/b]\n", 2, AuthzRefusal::RuleRepeated),
    (b"[groups]\n@g = a\n", 2, AuthzRefusal::GroupNameReserved),
    (b"[groups]\n$g = a\n", 2, AuthzRefusal::GroupNameReserved),
    (b"[groups]\n&g = a\n", 2, AuthzRefusal::GroupNameReserved),
    (b"[groups]\n*g = a\n", 2, AuthzRefusal::GroupNameReserved),
    (b"[groups]\n~g = a\n", 2, AuthzRefusal::GroupNameReserved),
    (b"[groups]\ng = a\ng = b\n", 3, AuthzRefusal::GroupRedefined),
    (b"[aliases]\n@a = x\n", 2, AuthzRefusal::AliasNameReserved),
    (
      b"[aliases]\na = x\na = y\n",
      3,
      AuthzRefusal::AliasRedefined,
    ),
    (b"[/]\n~~harry = r\n", 2, AuthzRefusal::EntryDoublyInverted),
    (b"[/]\n*x = r\n", 2, AuthzRefusal::EntryStarNotAlone),
    (b"[/]\n~* = r\n", 2, AuthzRefusal::EntryInvertedStar),
    (b"[/]\n$foo = r\n", 2, AuthzRefusal::EntryTokenInvalid),
    (b"[/]\n$ = r\n", 2, AuthzRefusal::EntryTokenInvalid),
    (b"[/]\n~$anon = r\n", 2, AuthzRefusal::EntryTokenInvalid),
    (b"[/]\nharry = R\n", 2, AuthzRefusal::AccessModeInvalid),
    (
      b"[/]\nharry = r # no trailing comments\n",
      2,
      AuthzRefusal::AccessModeInvalid,
    ),
    (b"[/]\nharry = w\n", 2, AuthzRefusal::AccessWriteOnly),
    (b"[/]\nharry = w w\n", 2, AuthzRefusal::AccessWriteOnly),
    (b"[groups]\ng = &nobody\n", 2, AuthzRefusal::AliasUndefined),
    (b"[/]\n&nobody = r\n", 2, AuthzRefusal::AliasUndefined),
    (b"[/]\n&= r\n", 2, AuthzRefusal::AliasUndefined),
    (b"[groups]\ng = @none\n", 2, AuthzRefusal::GroupUndefined),
    (b"[/]\n@none = r\n", 2, AuthzRefusal::EntryGroupUndefined),
    (b"[/]\n~@none = r\n", 2, AuthzRefusal::EntryGroupUndefined),
    (b"[groups]\ng = @g\n", 2, AuthzRefusal::GroupRecursive),
  ];
  for (bytes, line, kind) in cases {
    let refused = refusal(bytes);
    assert_eq!(
      refused,
      in_rules(*line, *kind),
      "{:?}",
      String::from_utf8_lossy(bytes)
    );
    assert_eq!(
      refused.subversion_code(),
      220_003,
      "SVN_ERR_AUTHZ_INVALID_CONFIG"
    );
  }
}

/// A file past the caller's bound is refused before a byte of it is read, the rules file's and the groups file's.
#[test]
fn a_file_past_the_bound_is_refused_whole() {
  let rules = b"[/]\n* = r\n";
  let tight = Bounds::of(rules, None);
  assert!(Authz::parse(rules, None, tight).is_ok());
  let short = Bounds {
    file_bytes: rules.len() - 1,
  };
  assert_eq!(
    Authz::parse(rules, None, short).expect_err("too large"),
    in_rules(1, AuthzRefusal::TooLarge)
  );
  let groups = b"[groups]\ng = a\n";
  assert_eq!(
    Authz::parse(rules, Some(groups), tight).expect_err("the groups file is longer"),
    in_groups(1, AuthzRefusal::TooLarge)
  );
  assert_eq!(
    Authz::parse(
      b"[/]\n\x00",
      Some(b"[groups]\x00"),
      Bounds { file_bytes: 64 }
    )
    .expect_err("nul"),
    in_rules(2, AuthzRefusal::NulByte),
    "the rules file is read before the groups file"
  );
}

/// The refusals after reading come at the line of the definition or entry that names them, the groups file's in it.
#[test]
fn refusals_after_reading_name_their_definition_or_entry() {
  assert_eq!(
    refusal(b"[groups]\na = x\nb = @c\nc = @d\nd = @b\n"),
    in_rules(3, AuthzRefusal::GroupRecursive),
    "the first group, in definition order, whose expansion never ends"
  );
  assert_eq!(
    refusal(b"[groups]\nlead = @b\nb = @c\nc = @b\n"),
    in_rules(2, AuthzRefusal::GroupRecursive),
    "a group that leads into a cycle never ends either"
  );
  assert_eq!(
    refusal_with_groups(b"[/]\n@g = r\n", b"[groups]\ng = &missing\n"),
    in_groups(2, AuthzRefusal::AliasUndefined)
  );
  assert_eq!(
    refusal_with_groups(b"[/]\nharry = r\n", b"[aliases]\n"),
    in_groups(1, AuthzRefusal::SectionInvalid)
  );
  assert_eq!(
    refusal_with_groups(b"[/]\nharry = r\n", b"[groups]\n[groups]\n"),
    in_groups(2, AuthzRefusal::SectionRepeated)
  );
}

/// An entry for a group with no members is ignored, as Subversion ignores it, whether inverted or not, and listed.
#[test]
fn an_entry_for_an_empty_group_is_ignored_and_listed() {
  let authz = parse("[groups]\nempty =\nalso = @empty\n[/]\n* = r\n@empty = rw\n~@also = rw\n");
  assert_eq!(
    authz.ignored_entries(),
    &[IgnoredEntry { line: 6 }, IgnoredEntry { line: 7 }]
  );
  assert_eq!(authz_access(&authz, b"", Some("anyone"), "/"), Access::Read);
}

// ---------------------------------------------------------------------------------------------------------------------
// Where Subversion's code and its own documentation part (the module documentation, items 1 to 3).

/// Item 1: the root has no segments, so `[:glob:/*]` does not decide it ("`*` matches any single non-empty path
/// segment", `authz.h`). Subversion 1.14.x's `lookup` walks `/` as one empty segment and answers r here.
#[test]
fn divergence_1_the_root_has_no_segments_so_a_star_rule_does_not_decide_it() {
  let authz = parse("[/]\nharry = rw\n[:glob:/*]\nharry = r\n");
  let rules = authz.rules(b"", principal(Some("harry")));
  assert_eq!(rules.access(path("/")), Access::Write);
  assert_eq!(rules.access(path("/top")), Access::Read);
  assert_eq!(
    rules.access(path("/top/deeper")),
    Access::Read,
    "inherited from the * level"
  );
}

/// Item 2: a suffix rule leaves the segment as it is for the rules beside it. Subversion 1.14.x's `lookup` reverses the
/// segment in place, so the `/**/public` rule, held by the next node of the level, meets `cilbup`: it misses
/// `/public` and matches `/cilbup`.
#[test]
fn divergence_2_a_suffix_rule_leaves_the_segment_for_the_rules_beside_it() {
  let authz = parse("[/]\n* =\n[:glob:/*.secret]\n* =\n[:glob:/**/public]\n* = r\n");
  let rules = authz.rules(b"", principal(None));
  assert_eq!(rules.access(path("/public")), Access::Read);
  assert_eq!(rules.access(path("/cilbup")), Access::None);
  assert_eq!(rules.access(path("/a/public")), Access::Read);
}

/// Item 3: a path that starts with two slashes is refused. Subversion 1.14.x takes `[//secret]` for the root's rule.
#[test]
fn divergence_3_two_slashes_at_the_start_of_a_rule_path_are_refused() {
  assert_eq!(
    refusal(b"[//secret]\n* =\n"),
    in_rules(1, AuthzRefusal::RulePathEmptySegment)
  );
  assert_eq!(
    refusal(b"[//]\n"),
    in_rules(1, AuthzRefusal::RulePathEmptySegment)
  );
}

// ---------------------------------------------------------------------------------------------------------------------
// The precedence, case by case.

/// A recursive question at the root weighs every rule beneath it.
#[test]
fn a_recursive_question_at_the_root_weighs_every_rule() {
  let authz = parse("[/]\n* = rw\n[/private]\n* =\n");
  let rules = authz.rules(b"", principal(None));
  assert_eq!(rules.access(path("/")), Access::Write);
  assert_eq!(rules.access_beneath(path("/")), Access::None);
  assert_eq!(rules.access_beneath(path("/public")), Access::Write);
}

/// The deepest level a rule matches decides, then the rule defined last; `**` matches zero segments too.
#[test]
fn the_deepest_matching_level_decides_then_the_last_rule() {
  let authz =
    parse("[:glob:/**/x]\nu = r\n[/a]\nu = rw\n[:glob:/a/*]\nu =\n[:glob:/a/**]\nu = r\n");
  let rules = authz.rules(b"", principal(Some("u")));
  assert_eq!(
    rules.access(path("/a")),
    Access::Read,
    "/a/** (later) and /a both match /a"
  );
  assert_eq!(
    rules.access(path("/a/b")),
    Access::Read,
    "/a/** (4) beats /a/* (3) at the same level"
  );
  assert_eq!(
    rules.access(path("/a/x")),
    Access::Read,
    "/a/** (4) beats /**/x (1) and /a/* (3)"
  );
  assert_eq!(rules.access(path("/b/x")), Access::Read);
  assert_eq!(rules.access(path("/b")), Access::None, "the root's default");
}

/// A later `**` eclipses the earlier rules beneath it for recursive questions too; an earlier one does not.
#[test]
fn a_later_double_star_eclipses_earlier_rules_beneath_it() {
  let later = parse("[/x/secret]\nu =\n[:glob:/x/**]\nu = rw\n");
  assert_eq!(
    later
      .rules(b"", principal(Some("u")))
      .access_beneath(path("/x")),
    Access::Write
  );
  let earlier = parse("[:glob:/x/**]\nu = rw\n[/x/secret]\nu =\n");
  let rules = earlier.rules(b"", principal(Some("u")));
  assert_eq!(rules.access_beneath(path("/x")), Access::None);
  assert_eq!(rules.access(path("/x/secret")), Access::None);
  assert_eq!(rules.access(path("/x/public")), Access::Write);
}

/// A question without a path takes the most any rule gives, even a rule another one overrides for the path.
#[test]
fn a_question_without_a_path_takes_the_most_any_rule_gives() {
  let authz = parse("[/]\nu = rw\n[R:/]\nu = r\n");
  let rules = authz.rules(b"R", principal(Some("u")));
  assert_eq!(rules.access(path("/")), Access::Read, "[R:/] speaks for u");
  assert_eq!(
    rules.access_anywhere(),
    Access::Write,
    "as svn_authz__get_global_rights accumulates"
  );
}

/// Inverted entries apply to authenticated users only; `~$anonymous` and `~$authenticated` swap.
#[test]
fn inverted_entries_apply_to_the_authenticated_they_do_not_name() {
  let authz = parse("[groups]\nstaff = alice\n[/]\n~bob = r\n~@staff = rw\n");
  assert_eq!(
    authz_access(&authz, b"", Some("alice"), "/"),
    Access::Read,
    "~bob"
  );
  assert_eq!(
    authz_access(&authz, b"", Some("bob"), "/"),
    Access::Write,
    "~@staff"
  );
  assert_eq!(authz_access(&authz, b"", Some("carol"), "/"), Access::Write);
  assert_eq!(
    authz_access(&authz, b"", None, "/"),
    Access::None,
    "never the anonymous user"
  );
}

/// The empty name is the anonymous user.
#[test]
fn the_empty_name_is_the_anonymous_user() {
  let authz = parse("[/]\n$anonymous = r\n$authenticated = rw\n");
  assert_eq!(Principal::named(Some(b"")), Principal::Anonymous);
  assert_eq!(
    authz.rules(b"", Principal::User(b"")).access(path("/")),
    Access::Read
  );
}

/// A path must start with `/` and hold no NUL; runs of `/` and a trailing one read as Subversion's `lookup` reads them.
#[test]
fn repository_paths_are_absolute_and_their_slashes_collapse() {
  assert_eq!(RepositoryPath::new(b""), Err(PathRefused::NotAbsolute));
  assert_eq!(RepositoryPath::new(b"a/b"), Err(PathRefused::NotAbsolute));
  assert_eq!(RepositoryPath::new(b"/a\x00b"), Err(PathRefused::NulByte));
  let authz = parse("[/a/b]\nu = r\n");
  let rules = authz.rules(b"", principal(Some("u")));
  for at in ["/a/b", "//a//b", "/a/b/", "/a/b//"] {
    assert_eq!(rules.access(path(at)), Access::Read, "{at}");
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// The uniform fast path's non-vacuity counter.

/// A principal whose rules all give one access is answered from it without a walk, and the counter shows it; one whose
/// rules differ is walked, and the counter stays still.
#[test]
fn the_uniform_fast_path_is_taken_and_counted() {
  let authz = parse("[/]\n* = r\n[/deep/path]\n* = r\n[/other]\nharry = rw\n");
  let uniform = authz.rules(b"", principal(Some("sally")));
  assert_eq!(uniform.uniform(), Some(Access::Read));
  assert_eq!(uniform.access(path("/deep/path/x")), Access::Read);
  assert_eq!(uniform.access_beneath(path("/")), Access::Read);
  assert_eq!(uniform.uniform_answers(), 2);
  let walked = authz.rules(b"", principal(Some("harry")));
  assert_eq!(walked.uniform(), None);
  assert_eq!(walked.access(path("/other")), Access::Write);
  assert_eq!(walked.access(path("/")), Access::Read);
  assert_eq!(walked.uniform_answers(), 0);
}

// ---------------------------------------------------------------------------------------------------------------------
// Hostile input.

/// A rule path of fifty thousand segments parses and answers without a stack to overflow.
#[test]
fn a_very_deep_rule_path_parses_and_answers() {
  let depth = 50_000;
  let deep_path: String = "/a".repeat(depth);
  let authz = parse(&format!("[/]\nu = r\n[{deep_path}]\nu = rw\n"));
  let rules = authz.rules(b"", principal(Some("u")));
  assert_eq!(rules.access(path(&deep_path)), Access::Write);
  assert_eq!(rules.access(path(&format!("{deep_path}/b"))), Access::Write);
  assert_eq!(rules.access(path("/a/a")), Access::Read);
  assert_eq!(rules.access_beneath(path("/")), Access::Read);
}

/// A chain of fifty thousand nested groups resolves without recursion, and closed into a cycle it is refused at its
/// first group.
#[test]
fn a_very_long_chain_of_groups_resolves_and_its_cycle_is_refused() {
  let length = 50_000;
  let mut groups = String::from("[groups]\n");
  for index in 0..length {
    groups.push_str(&format!("g{index} = @g{}\n", index + 1));
  }
  let chain = format!("{groups}g{length} = deep\n[/]\n@g0 = r\n");
  assert_eq!(
    authz_access(&parse(&chain), b"", Some("deep"), "/"),
    Access::Read
  );
  let cycle = format!("{groups}g{length} = @g0\n[/]\n@g0 = r\n");
  assert_eq!(
    refusal(cycle.as_bytes()),
    in_rules(2, AuthzRefusal::GroupRecursive)
  );
}

/// A pattern that a backtracking matcher would take exponential time over is matched in one pass per retry.
#[test]
fn a_pathological_pattern_matches_in_bounded_time() {
  let pattern = "*a*a*a*a*a*a*a*a*a*a*a*a*b";
  let long = "a".repeat(20_000);
  assert!(!glob::fnmatch(pattern.as_bytes(), long.as_bytes()));
  assert!(glob::fnmatch(
    pattern.as_bytes(),
    format!("{long}b").as_bytes()
  ));
}

/// Every prefix of a file that uses every construct parses or is refused by kind; none panics, and what parses
/// answers.
#[test]
fn every_truncation_of_a_full_file_parses_or_is_refused() {
  let full = AUTHZ_RULES;
  for length in 0..=full.len() {
    let prefix = &full[..length];
    if let Ok(authz) = Authz::parse(
      prefix,
      Some(AUTHZ_GROUPS),
      Bounds::of(prefix, Some(AUTHZ_GROUPS)),
    ) {
      let rules = authz.rules(b"bloop", principal(Some("luser")));
      let _ = rules.access(path("/xabc/defx/q/ghiXjkl/mno\\/z"));
      let _ = rules.access_beneath(path("/"));
    }
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// Arbitrary bytes are parsed or refused by kind, never a panic; what parses answers every question.
  #[test]
  fn arbitrary_bytes_parse_or_are_refused(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
    if let Ok(authz) = Authz::parse(&bytes, None, Bounds::of(&bytes, None)) {
      let rules = authz.rules(b"r", principal(Some("u")));
      let _ = rules.access(path("/a/b"));
      let _ = rules.access_beneath(path("/"));
      let _ = rules.access_anywhere();
    }
  }

  /// A valid file with one byte changed is parsed or refused by kind, never a panic.
  #[test]
  fn a_valid_file_with_one_byte_changed_parses_or_is_refused(at in 0usize..AUTHZ_RULES.len(), byte in any::<u8>()) {
    let mut bytes = AUTHZ_RULES.to_vec();
    bytes[at] = byte;
    if let Ok(authz) = Authz::parse(&bytes, Some(AUTHZ_GROUPS), Bounds::of(&bytes, Some(AUTHZ_GROUPS))) {
      let rules = authz.rules(b"bloop", principal(Some("luser")));
      let _ = rules.access(path("/xabc/defx/q"));
      let _ = rules.access_beneath(path("/xabc"));
    }
  }

  /// `apr_fnmatch` never panics and matches a pattern without wildcards exactly when the bytes are equal.
  #[test]
  fn fnmatch_never_panics_and_a_literal_pattern_is_equality(
    pattern in proptest::collection::vec(any::<u8>(), 0..24),
    string in proptest::collection::vec(any::<u8>(), 0..24),
  ) {
    let _ = glob::fnmatch(&pattern, &string);
    let literal: Vec<u8> = pattern.iter().copied().filter(|byte| !b"*?[]\\".contains(byte)).collect();
    prop_assert!(glob::fnmatch(&literal, &literal));
    prop_assert_eq!(glob::fnmatch(&literal, &string), literal == string);
  }
}
