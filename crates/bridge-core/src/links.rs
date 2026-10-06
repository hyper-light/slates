//! Symlinks that lead out of a volume (A-107): the volume's edge applies Linux's `protected_symlinks` rule (Linux
//! 3.6, `fs.protected_symlinks`; CWE-61, "UNIX symbolic link following") to every link whose target leaves it. Such a
//! link resolves for the caller who owns it and is refused `EACCES` to everyone else. An agent's own links (a venv's
//! absolute `python`) keep working for the agent; a link an agent planted cannot steer another user's write onto a
//! host path (`out -> /etc/cron.d/x`, followed by an operator's tool). A link that stays inside resolves for anyone,
//! so trees' relative links (`node_modules/.bin`, a venv's `lib64 -> lib`) are untouched.
//!
//! The kernel, not slates, follows a link, so the rule sits on what slates answers: the link's target, asked once per
//! follow (FUSE has no symlink cache unless `FUSE_CACHE_SYMLINKS` is negotiated, and slates does not ask for it).
//!
//! What "stays inside" means here is decided on the target text alone, conservatively: a relative target stays inside
//! only as leading `..` components no more than the link's directory depth, then plain names. A `..` after a name is
//! refused as leaving, because the name may itself be a link (`a/../..` with `a -> .` climbs one level further in the
//! kernel than in the text). Every link the kernel meets on the way is checked on its own when it is followed.

/// Whether `target`, the text of a symlink at `link_path` (the volume path of the link itself, `/`-separated from the
/// volume root), may lead out of the volume. Absolute targets always may; a relative one may when its leading `..`
/// components climb above the volume root, or when a `..` follows a name.
pub fn leaves_volume(link_path: &str, target: &str) -> bool {
  if target.starts_with('/') {
    return true;
  }
  let depth = link_path
    .split('/')
    .filter(|component| !component.is_empty())
    .count()
    .saturating_sub(1);
  let mut climbed = 0usize;
  let mut named = false;
  for component in target.split('/') {
    match component {
      "" | "." => {}
      ".." if named => return true,
      ".." => {
        climbed = climbed.saturating_add(1);
        if climbed > depth {
          return true;
        }
      }
      _ => named = true,
    }
  }
  false
}

#[cfg(test)]
mod tests {
  use super::leaves_volume;

  /// A-107: do classify absolute targets, targets that stay below the link, climbs within and above the root, and the
  /// `..`-after-a-name shape. Expect absolute and above-the-root targets to leave, a `..` after a name to count as
  /// leaving, and plain or in-bounds climbs to stay.
  #[test]
  fn targets_are_classified_by_where_they_can_lead() {
    for (link, target, leaves) in [
      ("/venv/bin/python", "/usr/bin/python3", true),
      ("/s", "/home/tester/out", true),
      ("/lib64", "lib", false),
      ("/node_modules/.bin/tsc", "../typescript/bin/tsc", false),
      ("/a/b/c", "../../x", false),
      ("/a/b/c", "../../../x", true),
      ("/top", "..", true),
      ("/a/b", "./././c", false),
      ("/a/b", "x/../y", true),
      ("/a/b/c", "../x/../y", true),
      ("/a/b", "", false),
      ("/a", "x//y", false),
    ] {
      assert_eq!(leaves_volume(link, target), leaves, "{link} -> {target}");
    }
  }
}
