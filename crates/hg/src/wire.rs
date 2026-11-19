//! A command's arguments, whichever transport carries them (A-569; `internals.wireprotocol` [B];
//! `wireprotov1server.py` [C]). Each command of 7.2's table names its arguments in a *spec*, a space-separated list in
//! which `*` stands for a dictionary of every argument the spec does not name: `cmds *` for `batch`, `one two *` for
//! `debugwireargs`, `*` for `getbundle`, nothing for `heads`. Over SSH each argument travels with its length
//! ([`crate::ssh`]); over HTTP as a form field ([`crate::http`]); either way the server reads them into
//! [`Arguments`]: the named ones in the spec's order, the dictionary's entries in the order they came.

/// Format: the name that stands for the dictionary of every argument a spec does not name.
pub const STAR: &[u8] = b"*";

/// Format: the byte between a spec's names.
const SEPARATOR: u8 = b' ';

/// A command's argument spec as 7.2's table states it, such as `b"one two *"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec<'s>(pub &'s [u8]);

impl<'s> Spec<'s> {
  /// Every name, in order, `*` among them where the spec has it.
  pub fn names(&self) -> impl Iterator<Item = &'s [u8]> + use<'s> {
    self
      .0
      .split(|byte| *byte == SEPARATOR)
      .filter(|name| !name.is_empty())
  }

  /// The named arguments, in order, `*` left out.
  pub fn named(&self) -> impl Iterator<Item = &'s [u8]> + use<'s> {
    self.names().filter(|name| *name != STAR)
  }

  /// Whether the spec takes a dictionary.
  pub fn takes_dictionary(&self) -> bool {
    self.names().any(|name| name == STAR)
  }

  /// Whether `name` is one of the spec's names, `*` included.
  pub fn has(&self, name: &[u8]) -> bool {
    self.names().any(|known| known == name)
  }

  /// The position of the named argument `name` among the named ones.
  pub fn position(&self, name: &[u8]) -> Option<usize> {
    self.named().position(|known| known == name)
  }

  /// The length of the longest name, `*` included: what an argument's name can take.
  pub fn longest_name(&self) -> usize {
    self.names().map(<[u8]>::len).max().unwrap_or(0)
  }
}

/// The arguments a command was sent, read for its spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arguments<'a> {
  /// Each named argument's value, in the spec's order.
  pub named: Vec<&'a [u8]>,
  /// The dictionary's entries, key and value, in the order they came; none where the spec takes no dictionary.
  pub dictionary: Vec<(&'a [u8], &'a [u8])>,
}

impl<'a> Arguments<'a> {
  /// The value of the named argument `name`, if `spec` names it.
  pub fn get(&self, spec: Spec<'_>, name: &[u8]) -> Option<&'a [u8]> {
    spec
      .position(name)
      .and_then(|at| self.named.get(at).copied())
  }

  /// The value of the dictionary's entry `key`, the first if it came twice.
  pub fn entry(&self, key: &[u8]) -> Option<&'a [u8]> {
    self
      .dictionary
      .iter()
      .find(|(known, _)| *known == key)
      .map(|(_, value)| *value)
  }
}

#[cfg(test)]
mod tests {
  use super::{Arguments, Spec};

  /// A-569: do read 7.2's specs for `debugwireargs`, `getbundle` and `heads`. Expect their names in order, the named
  /// ones without `*`, whether each takes a dictionary, and the longest name's length.
  #[test]
  fn a_spec_names_its_arguments_and_its_dictionary() {
    let wireargs = Spec(b"one two *");
    assert_eq!(
      wireargs.names().collect::<Vec<_>>(),
      [&b"one"[..], b"two", b"*"]
    );
    assert_eq!(wireargs.named().collect::<Vec<_>>(), [&b"one"[..], b"two"]);
    assert!(wireargs.takes_dictionary() && Spec(b"*").takes_dictionary());
    assert!(!Spec(b"").takes_dictionary() && Spec(b"").names().next().is_none());
    assert_eq!(
      (
        Spec(b"namespace key old new").longest_name(),
        Spec(b"").longest_name()
      ),
      (9, 0)
    );
  }

  /// A-569: do look arguments up by name and dictionary entries by key. Expect each value, and none for a name the
  /// spec does not have or a key that did not come.
  #[test]
  fn arguments_are_found_by_name_and_key() {
    let spec = Spec(b"one two *");
    let arguments = Arguments {
      named: vec![&b"un"[..], &b"deux"[..]],
      dictionary: vec![(&b"three"[..], &b"trois"[..])],
    };
    assert_eq!(
      (arguments.get(spec, b"two"), arguments.get(spec, b"three")),
      (Some(&b"deux"[..]), None)
    );
    assert_eq!(
      (arguments.entry(b"three"), arguments.entry(b"four")),
      (Some(&b"trois"[..]), None)
    );
  }
}
