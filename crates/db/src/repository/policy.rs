//! What the guard refuses of a repository's policy before the log records it (§4.17.1 "Policy"; A-622), and what one
//! is charged.
//!
//! A rule's pattern names what its repository's tool has (git's refs, Mercurial's branches, bookmarks and topics,
//! Subversion's paths), is not empty and holds no NUL, and a Subversion path starts at the root, `/path` or
//! `:glob:/path`; these are the same rules the server's matcher reads a pattern by (`slates_server`'s `vcs_policy`,
//! A-597), so a recorded policy always compiles. A rule's right is one its repository's tool has: the requirements
//! (merge commits, unsigned commits; A-644) are git's alone, the one push path that judges them, so a rule no push
//! would enforce never enters the log. A hook is at one of its repository's tool's points, under a name the
//! point allows (Mercurial's `pretxnchangegroup` or `pretxnchangegroup.suffix`, one hook a name; git's and Subversion's
//! the point's own name, one hook a point, as each keeps one hook file a point), and runs a program by an absolute path
//! holding no NUL. A policy is charged its rules', hooks' and trusted signers' encoded bytes, so an empty one costs
//! nothing. The signers are the server's to read (each an SSH key its verb parsed, A-644), as a volume's are (A-321).

use std::collections::BTreeSet;

use slates_wire::Wire;

use crate::catalog::{
  HookConfig, HookPointName, PolicyNames, PolicyRule, RepositoryPolicy, VcsTool,
};
use crate::error::DbError;

/// Format: the prefix of a Subversion glob rule's path (`[:glob:/path]`, the Subversion 1.10 release notes).
const GLOB_FORM: &str = ":glob:";

/// Refuses a policy for a repository serving `tool` that the log must never record: the first malformed rule, then the
/// first malformed hook, by index.
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
  Ok(())
}

/// The bytes a policy is charged: its rules', hooks' and trusted signers' encoded bytes, what the partition holds of it
/// beyond an empty one.
pub(super) fn bytes(policy: &RepositoryPolicy) -> usize {
  let rules = policy
    .rules
    .iter()
    .map(|rule| rule.to_bytes().len())
    .fold(0, usize::saturating_add);
  policy
    .hooks
    .iter()
    .map(|hook| hook.to_bytes().len())
    .fold(rules, usize::saturating_add)
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
    HookConfig, HookPointName, PolicyEffect, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
    RepositoryPolicy, VcsTool,
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
      signers: Vec::new(),
    }
  }

  /// A rule of `right`, denied to everyone, over git's `refs/heads/main` or another tool's own name for its main line.
  fn requirement(tool: VcsTool, right: PolicyRight) -> PolicyRule {
    let (names, pattern) = match tool {
      VcsTool::Git => (PolicyNames::Refs, "refs/heads/main"),
      VcsTool::Mercurial => (PolicyNames::Branches, "default"),
      VcsTool::Subversion => (PolicyNames::Paths, "/trunk"),
    };
    PolicyRule {
      right,
      effect: PolicyEffect::Deny,
      ..rule(names, pattern)
    }
  }

  /// A-644 (§4.17.2 "Protected branches": the requirements are git's push path's alone). Do: check a policy denying
  /// merge commits and one denying unsigned commits on each tool's main line, each after a rule every tool holds. Expect:
  /// git holds both; Mercurial and Subversion refuse each, naming its index (1), since no push of theirs would judge it.
  #[test]
  fn a_requirement_is_held_by_git_alone() {
    for right in [PolicyRight::MergeCommit, PolicyRight::UnsignedCommit] {
      let answers: Vec<Result<(), DbError>> =
        [VcsTool::Git, VcsTool::Mercurial, VcsTool::Subversion]
          .into_iter()
          .map(|tool| {
            let mut held = requirement(tool, PolicyRight::Submit);
            held.effect = PolicyEffect::Allow;
            check(
              tool,
              &policy(vec![held, requirement(tool, right)], Vec::new()),
            )
          })
          .collect();
      let refused = Err(DbError::PolicyRuleMalformed { index: 1 });
      assert_eq!(answers, vec![Ok(()), refused.clone(), refused], "{right:?}");
    }
  }

  /// A-644 (the charge). Do: weigh a policy of one trusted signer, and one of a rule and two signers. Expect: each
  /// signer's encoded bytes, beside the rule's, so a key list is held in the partition's budget as rules are.
  #[test]
  fn a_policy_is_charged_its_trusted_signers() {
    use slates_wire::Wire;
    let (first, second) = (vec![7u8; 51], vec![9u8; 83]);
    let signed = |rules: Vec<PolicyRule>, signers: Vec<Vec<u8>>| RepositoryPolicy {
      signers,
      ..policy(rules, Vec::new())
    };
    let one = rule(PolicyNames::Refs, "refs/heads/main");
    assert_eq!(
      bytes(&signed(Vec::new(), vec![first.clone()])),
      first.to_bytes().len()
    );
    assert_eq!(
      bytes(&signed(
        vec![one.clone()],
        vec![first.clone(), second.clone()]
      )),
      one.to_bytes().len() + first.to_bytes().len() + second.to_bytes().len()
    );
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
