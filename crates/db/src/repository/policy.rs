//! What the guard refuses of a repository's policy before the log records it (§4.17.1 "Policy"; A-622), and what one
//! is charged.
//!
//! A rule's pattern names what its repository's tool has (git's refs, Mercurial's branches, bookmarks and topics,
//! Subversion's paths), is not empty and holds no NUL, and a Subversion path starts at the root, `/path` or
//! `:glob:/path`; these are the same rules the server's matcher reads a pattern by (`slates_server`'s `vcs_policy`,
//! A-597), so a recorded policy always compiles. A hook is at one of its repository's tool's points, under a name the
//! point allows (Mercurial's `pretxnchangegroup` or `pretxnchangegroup.suffix`, one hook a name; git's and Subversion's
//! the point's own name, one hook a point, as each keeps one hook file a point), and runs a program by an absolute path
//! holding no NUL. A Subversion repository's hooks-env (A-648) holds sections named by something, holding no NUL, whose
//! variables are each one variable of a hook's environment: a name not empty, holding no `=` and no NUL, and a value
//! holding no NUL; any other repository holds none, since git's and Mercurial's hooks inherit the server's environment.
//! A policy is charged its rules', hooks' and hooks-env sections' encoded bytes, so an empty one costs nothing.

use std::collections::BTreeSet;

use slates_wire::Wire;

use crate::catalog::{
  HookConfig, HookPointName, HooksEnvSection, HooksEnvVariable, PolicyNames, PolicyRule,
  RepositoryPolicy, VcsTool,
};
use crate::error::DbError;

/// Format: the prefix of a Subversion glob rule's path (`[:glob:/path]`, the Subversion 1.10 release notes).
const GLOB_FORM: &str = ":glob:";

/// Refuses a policy for a repository serving `tool` that the log must never record: the first malformed rule, then the
/// first malformed hook, then the first malformed hooks-env section, by index.
pub(super) fn check(tool: VcsTool, policy: &RepositoryPolicy) -> Result<(), DbError> {
  if let Some(index) = policy.rules.iter().position(|rule| !rule_holds(tool, rule)) {
    return Err(DbError::PolicyRuleMalformed {
      index: index_of(index),
    });
  }
  let mut named: BTreeSet<(HookPointName, &str)> = BTreeSet::new();
  for (index, hook) in policy.hooks.iter().enumerate() {
    if !hook_holds(tool, hook) || !named.insert((hook.point, hook.name.as_str())) {
      return Err(DbError::PolicyHookMalformed {
        index: index_of(index),
      });
    }
  }
  if let Some(index) = policy
    .hooks_env
    .iter()
    .position(|section| !section_holds(tool, section))
  {
    return Err(DbError::PolicyHooksEnvMalformed {
      index: index_of(index),
    });
  }
  Ok(())
}

/// The bytes a policy is charged: its rules', hooks' and hooks-env sections' encoded bytes, what the partition holds of
/// it beyond an empty one.
pub(super) fn bytes(policy: &RepositoryPolicy) -> usize {
  let rules = policy
    .rules
    .iter()
    .map(|rule| rule.to_bytes().len())
    .fold(0, usize::saturating_add);
  let hooks = policy
    .hooks
    .iter()
    .map(|hook| hook.to_bytes().len())
    .fold(rules, usize::saturating_add);
  policy
    .hooks_env
    .iter()
    .map(|section| section.to_bytes().len())
    .fold(hooks, usize::saturating_add)
}

/// An item's index among its part of the policy, as a refusal names it.
fn index_of(index: usize) -> u32 {
  u32::try_from(index).unwrap_or(u32::MAX)
}

/// Whether `rule` is one a repository serving `tool` may hold (the module doc).
fn rule_holds(tool: VcsTool, rule: &PolicyRule) -> bool {
  let pattern = rule.pattern.as_str();
  rule.names.of(tool)
    && !pattern.is_empty()
    && !pattern.contains('\0')
    && (rule.names != PolicyNames::Paths
      || pattern
        .strip_prefix(GLOB_FORM)
        .unwrap_or(pattern)
        .starts_with('/'))
}

/// Whether `hook` is one a repository serving `tool` may hold, its name aside from the others' (the module doc).
fn hook_holds(tool: VcsTool, hook: &HookConfig) -> bool {
  hook.point.tool() == tool
    && named_for(hook.point, &hook.name)
    && !hook.program.contains('\0')
    && std::path::Path::new(&hook.program).is_absolute()
}

/// Whether `section` is one a repository serving `tool` may hold (the module doc): a Subversion repository's, named, and
/// each variable one a hook's environment can carry.
fn section_holds(tool: VcsTool, section: &HooksEnvSection) -> bool {
  tool == VcsTool::Subversion
    && !section.name.is_empty()
    && !section.name.contains('\0')
    && section.variables.iter().all(variable_holds)
}

/// Whether `variable` is one variable of a hook's environment: a name not empty, holding no `=` and no NUL, and a value
/// holding no NUL.
fn variable_holds(variable: &HooksEnvVariable) -> bool {
  !variable.name.is_empty() && !variable.name.contains(['=', '\0']) && !variable.value.contains(&0)
}

/// Whether `name` names a hook at `point`: at a Mercurial point, the point's name alone or with a `.suffix`; at a git or
/// Subversion point, the point's name alone.
fn named_for(point: HookPointName, name: &str) -> bool {
  let own = point.name();
  if point.tool() != VcsTool::Mercurial {
    return name == own;
  }
  name == own
    || name
      .strip_prefix(own)
      .and_then(|rest| rest.strip_prefix('.'))
      .is_some_and(|suffix| !suffix.is_empty())
}

#[cfg(test)]
mod tests {
  use super::{bytes, check};
  use crate::catalog::{
    HookConfig, HookPointName, HooksEnvSection, HooksEnvVariable, PolicyEffect, PolicyNames,
    PolicyRight, PolicyRule, PolicyWho, RepositoryPolicy, VcsTool,
  };
  use crate::error::DbError;

  /// A rule allowing everyone to submit, over `pattern` of `names`.
  fn rule(names: PolicyNames, pattern: &str) -> PolicyRule {
    PolicyRule {
      names,
      pattern: pattern.to_owned(),
      right: PolicyRight::Submit,
      effect: PolicyEffect::Allow,
      who: PolicyWho::Anyone,
    }
  }

  /// A hook at `point` named `name`, running `program`.
  fn hook(point: HookPointName, name: &str, program: &str) -> HookConfig {
    HookConfig {
      point,
      name: name.to_owned(),
      program: program.to_owned(),
    }
  }

  /// Shape: an absolute program path on the test's host.
  fn program() -> String {
    std::env::current_exe()
      .expect("the test binary's path")
      .to_string_lossy()
      .into_owned()
  }

  /// A policy of `rules` and `hooks`, version 1.
  fn policy(rules: Vec<PolicyRule>, hooks: Vec<HookConfig>) -> RepositoryPolicy {
    RepositoryPolicy {
      version: 1,
      rules,
      hooks,
      hooks_env: Vec::new(),
    }
  }

  /// A Subversion policy of nothing but the hooks-env `sections`.
  fn with_env(sections: Vec<HooksEnvSection>) -> RepositoryPolicy {
    RepositoryPolicy {
      hooks_env: sections,
      ..policy(Vec::new(), Vec::new())
    }
  }

  /// A hooks-env section `name` of `variables`, each a name and a value.
  fn env_section(name: &str, variables: &[(&str, &[u8])]) -> HooksEnvSection {
    HooksEnvSection {
      name: name.to_owned(),
      variables: variables
        .iter()
        .map(|(name, value)| HooksEnvVariable {
          name: (*name).to_owned(),
          value: value.to_vec(),
        })
        .collect(),
    }
  }

  /// A-648 (Subversion's nine points, `hooks.c`). Do: hold a hook at each of Subversion's points under its own name in a
  /// Subversion repository; hold each alone in a git repository; hold pre-lock named for post-lock. Expect: all nine
  /// held, named as Subversion names them; each refused in the git repository and the misnamed one refused, by index.
  #[test]
  fn subversions_nine_points_are_held_by_a_subversion_repository_alone() {
    let program = program();
    let points = [
      HookPointName::SvnStartCommit,
      HookPointName::SvnPreCommit,
      HookPointName::SvnPostCommit,
      HookPointName::SvnPreRevpropChange,
      HookPointName::SvnPostRevpropChange,
      HookPointName::SvnPreLock,
      HookPointName::SvnPostLock,
      HookPointName::SvnPreUnlock,
      HookPointName::SvnPostUnlock,
    ];
    let names: Vec<&str> = points.iter().map(|point| point.name()).collect();
    let expected = [
      "start-commit",
      "pre-commit",
      "post-commit",
      "pre-revprop-change",
      "post-revprop-change",
      "pre-lock",
      "post-lock",
      "pre-unlock",
      "post-unlock",
    ];
    assert_eq!(names, expected);
    let hooks: Vec<HookConfig> = points
      .iter()
      .map(|point| hook(*point, point.name(), &program))
      .collect();
    assert_eq!(
      check(VcsTool::Subversion, &policy(Vec::new(), hooks.clone())),
      Ok(())
    );
    for one in hooks {
      let name = one.name.clone();
      let refused = Err(DbError::PolicyHookMalformed { index: 0 });
      assert_eq!(
        check(VcsTool::Git, &policy(Vec::new(), vec![one])),
        refused,
        "{name}"
      );
    }
    let misnamed = hook(HookPointName::SvnPreLock, "post-lock", &program);
    assert_eq!(
      check(VcsTool::Subversion, &policy(Vec::new(), vec![misnamed])),
      Err(DbError::PolicyHookMalformed { index: 0 })
    );
  }

  /// A-648 (a hook's environment, `run_hook_cmd`). Do: hold a hooks-env of a default section, a pre-commit section and
  /// an empty post-commit section in a Subversion repository; then, each after a good section, sections a hook's
  /// environment cannot carry: named by nothing, a name holding a NUL, a variable named by nothing, one holding `=`,
  /// one holding a NUL, a value holding a NUL; and a good section in a git repository and a Mercurial one. Expect: the
  /// first held; each other refused naming its section's index.
  #[test]
  fn a_hooks_env_is_held_by_a_subversion_repository_alone_and_only_as_a_hook_can_take_it() {
    let good = env_section("default", &[("LANG", b"en_US.UTF-8")]);
    let svn = |sections: Vec<HooksEnvSection>| check(VcsTool::Subversion, &with_env(sections));
    let held = vec![
      good.clone(),
      env_section("pre-commit", &[("ONLY", b"pre-commit")]),
      env_section("post-commit", &[]),
    ];
    assert_eq!(svn(held), Ok(()));
    let bad = [
      env_section("", &[("A", b"1")]),
      env_section("pre\0commit", &[("A", b"1")]),
      env_section("default", &[("", b"1")]),
      env_section("default", &[("A=B", b"1")]),
      env_section("default", &[("A\0B", b"1")]),
      env_section("default", &[("A", b"1\02")]),
    ];
    for section in bad {
      let refused = Err(DbError::PolicyHooksEnvMalformed { index: 1 });
      assert_eq!(
        svn(vec![good.clone(), section.clone()]),
        refused,
        "{section:?}"
      );
    }
    for tool in [VcsTool::Git, VcsTool::Mercurial] {
      let refused = Err(DbError::PolicyHooksEnvMalformed { index: 0 });
      assert_eq!(
        check(tool, &with_env(vec![good.clone()])),
        refused,
        "{tool:?}"
      );
    }
  }

  /// A-648 (the merge budget). Do: charge a policy whose only item is one hooks-env section. Expect: that section's
  /// encoded bytes, as a rule or a hook is charged its own.
  #[test]
  fn a_hooks_env_section_is_charged_its_encoded_bytes() {
    let section = env_section("default", &[("LANG", b"en_US.UTF-8")]);
    let encoded = slates_wire::Wire::to_bytes(&section).len();
    assert_eq!(bytes(&with_env(vec![section])), encoded);
  }

  /// A-622 (the rules the log keeps). Do: check rules of every kind against each tool. Expect: git takes refs alone,
  /// Mercurial branches, bookmarks and topics, Subversion paths; an empty pattern, one holding a NUL and a Subversion
  /// path not from the root are refused, each by its index.
  #[test]
  fn a_rule_is_held_only_in_its_tools_terms() {
    let program = program();
    assert_eq!(
      check(
        VcsTool::Git,
        &policy(
          vec![rule(PolicyNames::Refs, "refs/heads/*")],
          vec![hook(HookPointName::GitPreReceive, "pre-receive", &program)]
        )
      ),
      Ok(())
    );
    for (tool, names) in [
      (VcsTool::Git, PolicyNames::Branches),
      (VcsTool::Mercurial, PolicyNames::Refs),
      (VcsTool::Mercurial, PolicyNames::Paths),
      (VcsTool::Subversion, PolicyNames::Topics),
    ] {
      assert_eq!(
        check(tool, &policy(vec![rule(names, "x")], Vec::new())),
        Err(DbError::PolicyRuleMalformed { index: 0 }),
        "{names:?} in a {tool:?} repository"
      );
    }
    for names in [
      PolicyNames::Branches,
      PolicyNames::Bookmarks,
      PolicyNames::Topics,
    ] {
      assert_eq!(
        check(
          VcsTool::Mercurial,
          &policy(vec![rule(names, "*")], Vec::new())
        ),
        Ok(())
      );
    }
    let svn = |pattern: &str| {
      check(
        VcsTool::Subversion,
        &policy(
          vec![
            rule(PolicyNames::Paths, "/trunk"),
            rule(PolicyNames::Paths, pattern),
          ],
          Vec::new(),
        ),
      )
    };
    assert_eq!(svn(":glob:/tags/**"), Ok(()));
    assert_eq!(svn("trunk"), Err(DbError::PolicyRuleMalformed { index: 1 }));
    assert_eq!(
      svn(":glob:tags"),
      Err(DbError::PolicyRuleMalformed { index: 1 })
    );
    assert_eq!(svn(""), Err(DbError::PolicyRuleMalformed { index: 1 }));
    assert_eq!(svn("/a\0b"), Err(DbError::PolicyRuleMalformed { index: 1 }));
  }

  /// A-622 (the hooks the log keeps). Do: check hooks at each tool's points under each kind of name and program. Expect:
  /// a hook at another tool's point, a git or Subversion hook not named for its point, a second hook at one git point,
  /// a Mercurial name repeated or not its point's, and a relative program or one holding a NUL are refused by index;
  /// Mercurial's suffixed names at one point are held.
  #[test]
  fn a_hook_is_held_only_at_its_tools_points_under_a_name_its_point_allows() {
    let program = program();
    let hg = |hooks: Vec<HookConfig>| check(VcsTool::Mercurial, &policy(Vec::new(), hooks));
    assert_eq!(
      hg(vec![
        hook(
          HookPointName::HgPretxnchangegroup,
          "pretxnchangegroup",
          &program
        ),
        hook(
          HookPointName::HgPretxnchangegroup,
          "pretxnchangegroup.acl",
          &program
        ),
        hook(HookPointName::HgChangegroup, "changegroup.notify", &program),
      ]),
      Ok(())
    );
    assert_eq!(
      hg(vec![
        hook(
          HookPointName::HgPretxnchangegroup,
          "pretxnchangegroup.acl",
          &program
        ),
        hook(
          HookPointName::HgPretxnchangegroup,
          "pretxnchangegroup.acl",
          &program
        ),
      ]),
      Err(DbError::PolicyHookMalformed { index: 1 })
    );
    for name in ["changegroup", "pretxnchangegroup.", "pretxnchangegroupx"] {
      assert_eq!(
        hg(vec![hook(
          HookPointName::HgPretxnchangegroup,
          name,
          &program
        )]),
        Err(DbError::PolicyHookMalformed { index: 0 }),
        "{name}"
      );
    }
    let git = |hooks: Vec<HookConfig>| check(VcsTool::Git, &policy(Vec::new(), hooks));
    assert_eq!(
      git(vec![
        hook(HookPointName::GitPreReceive, "pre-receive", &program),
        hook(HookPointName::GitPreReceive, "pre-receive", &program),
      ]),
      Err(DbError::PolicyHookMalformed { index: 1 })
    );
    assert_eq!(
      git(vec![hook(
        HookPointName::GitUpdate,
        "update.check",
        &program
      )]),
      Err(DbError::PolicyHookMalformed { index: 0 })
    );
    assert_eq!(
      git(vec![hook(
        HookPointName::SvnPreCommit,
        "pre-commit",
        &program
      )]),
      Err(DbError::PolicyHookMalformed { index: 0 })
    );
    for bad in ["hooks/pre-receive", "", "/bin/a\0b"] {
      assert_eq!(
        git(vec![hook(HookPointName::GitPreReceive, "pre-receive", bad)]),
        Err(DbError::PolicyHookMalformed { index: 0 }),
        "{bad:?}"
      );
    }
    assert_eq!(
      check(
        VcsTool::Subversion,
        &policy(
          Vec::new(),
          vec![hook(
            HookPointName::SvnPreRevpropChange,
            "pre-revprop-change",
            &program
          )]
        )
      ),
      Ok(())
    );
  }

  /// A-622 (the charge). Do: weigh an empty policy, one rule, and one rule and one hook. Expect: nothing for the empty
  /// one, whatever its version, and each item's encoded bytes for the others.
  #[test]
  fn a_policy_is_charged_its_rules_and_hooks_and_an_empty_one_nothing() {
    use slates_wire::Wire;
    assert_eq!(bytes(&RepositoryPolicy::default()), 0);
    assert_eq!(
      bytes(&RepositoryPolicy {
        version: 7,
        ..RepositoryPolicy::default()
      }),
      0
    );
    let one = rule(PolicyNames::Refs, "refs/heads/main");
    let hook = hook(HookPointName::GitUpdate, "update", "/srv/hooks/update");
    assert_eq!(
      bytes(&policy(vec![one.clone()], Vec::new())),
      one.to_bytes().len()
    );
    assert_eq!(
      bytes(&policy(vec![one.clone()], vec![hook.clone()])),
      one.to_bytes().len() + hook.to_bytes().len()
    );
  }
}
