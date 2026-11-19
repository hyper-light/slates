//! Ref names, checked as git checks them (A-615; §4.17.2 "The repository in git's terms", item 6): one rule for the
//! whole crate, the rule of git's own `check_refname_format` with no flags [C: `refs.c` `refname_disposition`,
//! `check_refname_component` and `check_or_sanitize_refname`, v2.50.1], which `git check-ref-format` documents and
//! gitprotocol-common lists [B: "refname"].
//!
//! Two callers ask it two questions. Protocol v2's lines carry whole names ([`is_valid`]): `HEAD`, or a name under
//! `refs/` (A-582 part 2: `want-ref`, `ls-refs`'s answer and its symref targets, `wanted-refs`). A tag object names its
//! tag without the `refs/tags/` git checks it under ([`tag_name_is_valid`]; A-590, git's `badTagName`). Both are the
//! same walk over a name's components; before A-615 each module kept its own copy.
//!
//! **The rule.** A name is cut at each `/` into components. Each component is not empty, does not begin with `.`, does
//! not end with `.lock`, holds no `..` and no `@{`, and holds no byte `refname_disposition` forbids: the ASCII control
//! bytes and DEL, space, `:`, `?`, `[`, `\`, `^`, `~`, and `*` (a pattern's, never a name's). The whole name does not
//! end with `.`. Bytes past ASCII are allowed, as git allows them. The document's own list leaves out two of these,
//! which no ref git makes can break: no empty component (`refs//x`, `refs/x/`) and no component, rather than the whole
//! name, ending with `.lock`. A NUL ends a name's string in git's C, so no ref git holds contains one, and a name
//! holding one is refused. git's two rules about the whole name, that it is not `@` alone and has two components at
//! least, can never fail here: a whole name is `HEAD` or begins `refs/`, and a tag's is checked under `refs/tags/`.

/// Format: the one name outside `refs/` the protocol carries [gitprotocol-common: `refname = "HEAD"`].
const HEAD: &[u8] = b"HEAD";

/// Format: what every other whole name begins with [gitprotocol-common: `refname /= "refs/" <see discussion below>`].
const REFS: &[u8] = b"refs/";

/// Format: the byte between a name's components (`refname_disposition` 1, with the NUL that ends a C string).
const SLASH: u8 = b'/';

/// Format: no component begins with it, and no name ends with it.
const DOT: u8 = b'.';

/// Format: no component ends with it (`refs.c`'s `LOCK_SUFFIX`).
const LOCK: &[u8] = b".lock";

/// Format: the pairs no component holds (`refname_disposition` 2 and 3): `..`, and `@{`.
const PAIRS: [&[u8]; 2] = [b"..", b"@{"];

/// Format: the printable bytes no name holds (`refname_disposition` 4, and 5 refused as a name's rather than a
/// pattern's): space, `:`, `?`, `[`, `\`, `^`, `~` and `*`.
const FORBIDDEN: &[u8] = b" :?[\\^~*";

/// Format: the length of a pair [`PAIRS`] holds.
const PAIR_LEN: usize = 2;

/// Whether `name` is a whole ref name the protocol may carry: `HEAD`, or a name under `refs/` that git's
/// `check_refname_format` passes.
pub fn is_valid(name: &[u8]) -> bool {
  name == HEAD || (name.starts_with(REFS) && components_are_valid(name))
}

/// Whether `name`, a tag object's `tag` line, names a tag git accepts: `refs/tags/<name>` passes git's
/// `check_refname_format` (`fsck.c`'s `badTagName` check). `refs` and `tags` pass, so the name's own components decide.
pub fn tag_name_is_valid(name: &[u8]) -> bool {
  components_are_valid(name)
}

/// Whether every component of `name` passes and the name does not end with `.`.
fn components_are_valid(name: &[u8]) -> bool {
  name.last() != Some(&DOT) && name.split(|byte| *byte == SLASH).all(component_is_valid)
}

/// Whether one component passes: not empty, not beginning with a dot, not ending with `.lock`, no forbidden byte and
/// no forbidden pair.
fn component_is_valid(component: &[u8]) -> bool {
  !component.is_empty()
    && component.first() != Some(&DOT)
    && !component.ends_with(LOCK)
    && !component.iter().any(|byte| is_forbidden(*byte))
    && !component
      .windows(PAIR_LEN)
      .any(|pair| PAIRS.contains(&pair))
}

/// Whether `byte` is one no name holds: an ASCII control byte (the NUL among them) or DEL, or a forbidden printable
/// one.
fn is_forbidden(byte: u8) -> bool {
  byte.is_ascii_control() || FORBIDDEN.contains(&byte)
}

#[cfg(test)]
#[path = "tests/refname.rs"]
mod tests;
