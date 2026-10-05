//! The slates skills over MCP (§4.12, D-19): the `SKILL.md` documents of the repository's `skills/` tree, compiled
//! into the binary so the one `slates` binary serves them, published three ways from one source as D-19 decided —
//! as `skill://slates/<name>/SKILL.md` resources (`text/markdown`; a client pulls one into context), as prompts of
//! the same name (a user invokes one, Claude Code as `/mcp__slates__<name>`), and through `slates.help`.
//!
//! Each document follows the open Agent Skills specification (agentskills.io): YAML frontmatter with `name` (the
//! directory's name) and `description` (what it does and when to use it), then the instructions. The resource's
//! description is the frontmatter's, so a client's catalog shows what the skill itself says.

/// Format: the URI scheme and authority every skill resource is named under.
pub(crate) const URI_PREFIX: &str = "skill://slates/";
/// Format: the file every skill resource names inside its skill's directory.
pub(crate) const URI_FILE: &str = "/SKILL.md";
/// Format: the RFC 6570 template naming any skill resource.
pub(crate) const URI_TEMPLATE: &str = "skill://slates/{name}/SKILL.md";
/// Format: the media type of a skill document (the specification defines none of its own).
pub(crate) const MIME_TYPE: &str = "text/markdown";

/// One skill: its name (its directory's) and its document.
pub(crate) struct Skill {
  /// The skill's name, as its frontmatter and directory give it.
  pub(crate) name: &'static str,
  /// The whole `SKILL.md`.
  pub(crate) body: &'static str,
}

/// Format: the skills this binary serves, in a fixed order (MCP 2026-07-28: lists SHOULD be deterministic).
pub(crate) const SKILLS: &[Skill] = &[
  Skill {
    name: "working-in-slates-volumes",
    body: include_str!("../../../skills/working-in-slates-volumes/SKILL.md"),
  },
  Skill {
    name: "merging-work-in-slates",
    body: include_str!("../../../skills/merging-work-in-slates/SKILL.md"),
  },
  Skill {
    name: "landing-slates-work-to-disk",
    body: include_str!("../../../skills/landing-slates-work-to-disk/SKILL.md"),
  },
];

impl Skill {
  /// The skill's resource URI.
  pub(crate) fn uri(&self) -> String {
    format!("{URI_PREFIX}{}{URI_FILE}", self.name)
  }

  /// The frontmatter's `description`: the text after `description:` on its line inside the leading `---` block.
  pub(crate) fn description(&self) -> &'static str {
    frontmatter(self.body)
      .lines()
      .find_map(|line| line.strip_prefix("description:"))
      .map(str::trim)
      .unwrap_or_default()
  }
}

/// The skill a resource URI names, if any.
pub(crate) fn by_uri(uri: &str) -> Option<&'static Skill> {
  let name = uri.strip_prefix(URI_PREFIX)?.strip_suffix(URI_FILE)?;
  by_name(name)
}

/// The skill named `name`, if any.
pub(crate) fn by_name(name: &str) -> Option<&'static Skill> {
  SKILLS.iter().find(|skill| skill.name == name)
}

/// The YAML between a document's leading `---` line and the next one (empty when it has none).
pub(crate) fn frontmatter(body: &str) -> &str {
  body
    .strip_prefix("---\n")
    .and_then(|rest| rest.split_once("\n---\n"))
    .map(|(yaml, _)| yaml)
    .unwrap_or_default()
}
