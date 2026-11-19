//! What receive-pack hands a hook beside the repository (A-665): `GIT_DIR`, and for `pre-receive`
//! the three variables git's quarantine sets, each as stock git 2.50.1 sets it
//! (`fixtures/view/hook.*.txt`, made by `make-view-fixtures.sh`).
//!
//! **The quarantine** (`tmp-objdir.c` `tmp_objdir_create`). receive-pack makes
//! `<objects>/tmp_objdir-incoming-XXXXXX` with `mkdtemp`, puts the push's objects there, and
//! hands `pre-receive` `GIT_OBJECT_DIRECTORY` and `GIT_QUARANTINE_PATH`, both that directory, and
//! `GIT_ALTERNATE_OBJECT_DIRECTORIES`, the repository's own object directory, so the hook's `git`
//! reads the push's objects and the repository's alike (`env_replace`, `env_append`). Each is
//! `absolute_path` of the object directory receive-pack names from `GIT_DIR=.`, `./objects`: the
//! working directory, then `/./objects`. The view names its quarantine with the first six digits
//! of its pack's name in place of `mkdtemp`'s six random letters, so the name is the same for the
//! same push and nothing random enters it. `update` and `post-receive` run after receive-pack has
//! moved the objects into the repository (`tmp_objdir_migrate`, `execute_commands`), so neither
//! sees a quarantine: both see `GIT_DIR=.` alone.
//!
//! **Quoting.** `env_append` wraps a value in double quotes and quotes it as C does
//! (`quote_c_style`) when it begins with a double quote or holds `PATH_SEP`, the separator of a
//! list of directories, so a path holding one stays one directory. With git's default
//! `core.quotePath` a byte past ASCII is quoted too, in octal (`quote.c` `cq_lookup`).
//!
//! **Left out.** `GIT_EXEC_PATH` (the hook's own `git` knows where it is installed) and the push's
//! options (`GIT_PUSH_OPTION_*`, which the hook driver sets with the rest of a hook's input).

use crate::object_name::ObjectId;

/// Format: the object directory receive-pack names, `GIT_DIR` being `.`, which `absolute_path`
/// puts after the working directory and a `/` (`abspath.c` `strbuf_add_absolute_path`).
const OBJECTS: &[u8] = b"./objects";

/// Format: `GIT_DIR` as receive-pack exports it to every hook, after entering the repository
/// (`setup.c` `set_git_dir_1`; every hook's listing in `fixtures/view`).
const GIT_DIR: &[u8] = b".";

/// Format: the quarantine's directory: `tmp_objdir-`, the prefix receive-pack gives,
/// `incoming`, a `-`, then `mkdtemp`'s six letters (`tmp_objdir_create`).
const QUARANTINE: &[u8] = b"tmp_objdir-incoming-";

/// Format: the letters `mkdtemp` puts after the quarantine's prefix (`XXXXXX`).
const QUARANTINE_LETTERS: usize = 6;

/// Format: git's `PATH_SEP` on POSIX systems, between the directories of a list
/// (`git-compat-util.h`).
#[cfg(not(windows))]
const PATH_SEP: u8 = b':';

/// Format: git's `PATH_SEP` on Windows (`compat/mingw.h`).
#[cfg(windows)]
const PATH_SEP: u8 = b';';

/// Format: the bytes `quote_c_style` writes with a backslash and a letter (`quote.c` `cq_lookup`):
/// BEL, BS, HT, LF, VT, FF and CR, the double quote and the backslash.
const LETTERED: [(u8, u8); 9] = [
  (0x07, b'a'),
  (0x08, b'b'),
  (b'\t', b't'),
  (b'\n', b'n'),
  (0x0b, b'v'),
  (0x0c, b'f'),
  (b'\r', b'r'),
  (b'"', b'"'),
  (b'\\', b'\\'),
];

/// Format: the bits each octal digit of a quoted byte carries, the first digit the byte's top two
/// (`quote_c_style_counted`: `(ch >> 6) & 03`, `(ch >> 3) & 07`, `ch & 07`).
const OCTAL_SHIFTS: [u32; 3] = [6, 3, 0];

/// Format: an octal digit's mask.
const OCTAL_DIGIT: u8 = 0o7;

/// The quarantine directory's name for the push whose pack is named `pack`.
pub(super) fn quarantine_name(pack: &ObjectId) -> Vec<u8> {
  let mut hex = Vec::new();
  pack.write_hex(&mut hex);
  let mut name = QUARANTINE.to_vec();
  name.extend_from_slice(hex.get(..QUARANTINE_LETTERS).unwrap_or(hex.as_slice()));
  name
}

/// The variables a hook running in the view rooted at `root` is handed, sorted by name: `GIT_DIR`,
/// and with `quarantine` (the quarantine directory's name, for `pre-receive`) the quarantine's
/// three (module docs).
pub(super) fn environment(root: &[u8], quarantine: Option<&[u8]>) -> Vec<(&'static str, Vec<u8>)> {
  let Some(name) = quarantine else {
    return vec![("GIT_DIR", GIT_DIR.to_vec())];
  };
  let objects = absolute(root, OBJECTS);
  let mut quarantined = objects.clone();
  quarantined.push(b'/');
  quarantined.extend_from_slice(name);
  vec![
    ("GIT_ALTERNATE_OBJECT_DIRECTORIES", appended(&objects)),
    ("GIT_DIR", GIT_DIR.to_vec()),
    ("GIT_OBJECT_DIRECTORY", quarantined.clone()),
    ("GIT_QUARANTINE_PATH", quarantined),
  ]
}

/// `path` after `root` as `strbuf_add_absolute_path` puts a relative path after the working
/// directory: a `/` between them unless `root` ends with one.
fn absolute(root: &[u8], path: &[u8]) -> Vec<u8> {
  let mut joined = root.to_vec();
  if joined.last().is_some_and(|last| *last != b'/') {
    joined.push(b'/');
  }
  joined.extend_from_slice(path);
  joined
}

/// `value` as `env_append` sets a variable that held nothing before: quoted (module docs) when it
/// begins with a double quote or holds `PATH_SEP`, as it is otherwise.
fn appended(value: &[u8]) -> Vec<u8> {
  if value.first() != Some(&b'"') && !value.contains(&PATH_SEP) {
    return value.to_vec();
  }
  let mut quoted = vec![b'"'];
  quoted.extend(c_quoted(value));
  quoted.push(b'"');
  quoted
}

/// `value` as `quote_c_style` quotes it without its own double quotes (`CQUOTE_NODQ`): each byte
/// `cq_lookup` names a letter for as a backslash and that letter, each other control byte, DEL
/// and each byte past ASCII as a backslash and three octal digits, the rest as it is.
fn c_quoted(value: &[u8]) -> Vec<u8> {
  let mut quoted = Vec::with_capacity(value.len());
  for byte in value {
    if let Some((_, letter)) = LETTERED.iter().find(|(lettered, _)| lettered == byte) {
      quoted.extend_from_slice(&[b'\\', *letter]);
    } else if byte.is_ascii_control() || !byte.is_ascii() {
      quoted.push(b'\\');
      for shift in OCTAL_SHIFTS {
        let digit = byte.checked_shr(shift).unwrap_or(0) & OCTAL_DIGIT;
        quoted.push(b'0' | digit);
      }
    } else {
      quoted.push(*byte);
    }
  }
  quoted
}
