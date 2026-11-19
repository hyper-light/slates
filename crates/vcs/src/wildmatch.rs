//! git's `wildmatch` with `WM_PATHNAME` (§4.17.1 "Policy"; A-597): the glob git matches a ref name with
//! (`git for-each-ref`'s patterns, `ref-filter.c` `match_name_as_path` at git e1ec09b23), which a git repository's
//! policy patterns use, and which a Subversion glob rule's segments use within one segment (slates-server's
//! `vcs_policy`).
//!
//! **Where it lives.** Here, in the crate every version-control server's crate builds on, so that each one matching a
//! pattern holds this one matcher and no copy of it (A-671): it was born in slates-server with the policy (A-597), and
//! git's own crate needs git's glob for a sparse filter's patterns (A-640 part 4). Its oracle is a file of its own
//! (`wildmatch/reference.rs`), which slates-server's policy tests compile too, so both crates' tests hold one
//! transcription and no product build links it.
//!
//! **The rule, as git's `dowild` has it** (`wildmatch.c` at e1ec09b23; `gitignore.adoc`, "Two consecutive
//! asterisks"). `?` is one byte other than `/`. A bracket expression is one byte other than `/` in its set: `!` or `^`
//! first negates it, `a-z` is a range, `\` makes the next byte a member, and `[:alpha:]` and its eleven siblings are
//! read over git's own ASCII table (`ctype.c`: nothing at 128 or above is in any class, and space is tab, newline,
//! carriage return and space alone). `*` is any run of bytes holding no `/`. `**` is the same, unless it stands alone
//! between a slash or the pattern's start and a slash or the pattern's end: at the end it is any run at all, and before
//! a slash it is nothing or any run ending in a slash ("matches zero or more directories"). `\` makes the next byte
//! literal. A bracket expression that never closes or names a class git does not know, and a lone `\` at the
//! pattern's end, match nothing: `dowild` answers them `WM_ABORT_ALL` or compares the text with the pattern's end.
//!
//! **The shape.** `dowild` recurses at every star and backtracks, its `WM_ABORT_*` answers pruning the search. Here a
//! pattern is read once into tokens ([`Glob::new`]) and matched by carrying the set of text positions the tokens so far
//! can reach ([`Glob::matches`]): time in proportion to the tokens times the text, one set of the text's length plus
//! one, and no recursion, so no pattern, however long or starry, can run the stack out or take exponential time. Two
//! oracles hold it to git: a line-for-line transcription of `dowild` (the tests' `reference`), compared on generated
//! patterns and texts, and git's own 190 vectors (`t/t3070-wildmatch.sh` at e1ec09b23, the case-sensitive
//! `WM_PATHNAME` column, with `t/helper/test-wildmatch.c`'s `XXX/` rewriting applied), held by both.
//!
//! **Bytes.** git's strings end at a NUL; here a NUL is one more byte. No ref name or path segment holds one (the
//! namespace refuses it, A-515), and the policy refuses a pattern that does (slates-server's `vcs_policy`).

/// A pattern read into tokens, matched against any number of texts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Glob {
  /// The tokens, in the pattern's order.
  tokens: Vec<Token>,
}

/// One step of a compiled pattern (the module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
  /// This byte.
  Byte(u8),
  /// `?`: one byte other than `/`.
  One,
  /// A bracket expression: one byte of the set, which never holds `/`.
  Set(ByteSet),
  /// What `dowild` never matches: a bracket expression cut short or naming a class git does not know, or a lone `\`
  /// at the pattern's end.
  Never,
  /// `*`, and `**` away from a segment's edges: any run of bytes holding no `/`.
  Run,
  /// `**` alone in its segment at the pattern's end, or before an escaped slash: any run of bytes.
  RunAll,
  /// `**/` alone in its segment: nothing, or any run of bytes ending in `/`.
  Dirs,
}

/// Whether `byte` is one of git's glob specials (`is_glob_special`: `ctype.c`'s `G` class, `*`, `?`, `[` and `\`): a
/// pattern holding none is matched literally.
pub fn is_glob_special(byte: u8) -> bool {
  matches!(byte, b'*' | b'?' | b'[' | b'\\')
}

impl Glob {
  /// `pattern` read as `dowild` reads it (the module doc). Every pattern compiles: what git never matches becomes a
  /// token that matches nothing.
  pub fn new(pattern: &[u8]) -> Self {
    let mut tokens = Vec::new();
    let mut at = 0;
    while let Some(&byte) = pattern.get(at) {
      let (token, next) = match byte {
        b'\\' => escaped(pattern, at),
        b'?' => (Token::One, at.saturating_add(1)),
        b'*' => stars(pattern, at),
        b'[' => bracket(pattern, at),
        _ => (Token::Byte(byte), at.saturating_add(1)),
      };
      tokens.push(token);
      at = next;
    }
    Self { tokens }
  }

  /// Whether the pattern matches the whole of `text`: the positions each token can reach from the positions the
  /// tokens before it reached, starting from the text's start, and a match when its end is reached.
  pub fn matches(&self, text: &[u8]) -> bool {
    let Some(width) = text.len().checked_add(1) else {
      return false;
    };
    let mut reached = vec![false; width];
    let mut next = vec![false; width];
    if let Some(start) = reached.first_mut() {
      *start = true;
    }
    for token in &self.tokens {
      advance(token, text, &reached, &mut next);
      std::mem::swap(&mut reached, &mut next);
      if !reached.contains(&true) {
        return false;
      }
    }
    reached.last().copied().unwrap_or(false)
  }

  /// The bytes the compiled pattern keeps beyond its own size: its tokens' room, which holds nothing on the heap of its
  /// own. A caller that keeps a pattern charges these to the request it serves (A-640 part 4: a sparse filter's
  /// patterns, charged to the fetch's admission).
  pub fn kept_bytes(&self) -> u64 {
    let room = self
      .tokens
      .capacity()
      .saturating_mul(core::mem::size_of::<Token>());
    u64::try_from(room).unwrap_or(u64::MAX)
  }
}

/// The positions `token` reaches in `text` from the positions in `from`, written into `to` (each of the text's length
/// plus one).
fn advance(token: &Token, text: &[u8], from: &[bool], to: &mut [bool]) {
  match token {
    Token::Byte(expected) => one_byte(text, from, to, |byte| byte == *expected),
    Token::One => one_byte(text, from, to, |byte| byte != b'/'),
    Token::Set(set) => one_byte(text, from, to, |byte| set.contains(byte)),
    Token::Never => to.fill(false),
    Token::Run => run(text, from, to, false),
    Token::RunAll => run(text, from, to, true),
    Token::Dirs => directories(text, from, to),
  }
}

/// One byte that `accepts` takes: position `at + 1` is reached when `at` was and its byte is taken.
fn one_byte(text: &[u8], from: &[bool], to: &mut [bool], accepts: impl Fn(u8) -> bool) {
  to.fill(false);
  for (at, (&byte, &was)) in text.iter().zip(from).enumerate() {
    if was && accepts(byte) {
      if let Some(slot) = to.get_mut(at.saturating_add(1)) {
        *slot = true;
      }
    }
  }
}

/// A run of bytes, `/` among them only when `crosses_slash`: a position is reached when it was, or when the position
/// before it was reached by the run and its byte may be in the run.
fn run(text: &[u8], from: &[bool], to: &mut [bool], crosses_slash: bool) {
  let mut carried = false;
  for (at, slot) in to.iter_mut().enumerate() {
    *slot = carried || from.get(at).copied().unwrap_or(false);
    carried = *slot
      && text
        .get(at)
        .is_some_and(|&byte| crosses_slash || byte != b'/');
  }
}

/// `**/`: a position is reached when it was (no directory), or when some earlier position was and the byte just
/// before it is `/` (the `**` took everything up to that slash).
fn directories(text: &[u8], from: &[bool], to: &mut [bool]) {
  let mut earlier = false;
  for (at, slot) in to.iter_mut().enumerate() {
    let here = from.get(at).copied().unwrap_or(false);
    let after_slash = at
      .checked_sub(1)
      .and_then(|before| text.get(before))
      .is_some_and(|&byte| byte == b'/');
    *slot = here || (earlier && after_slash);
    earlier = earlier || here;
  }
}

/// `\` at `at`: the next byte, literal; a `\` that ends the pattern matches nothing (`dowild` compares the text with
/// the pattern's terminating NUL).
fn escaped(pattern: &[u8], at: usize) -> (Token, usize) {
  let next = at.saturating_add(1);
  match pattern.get(next) {
    Some(&byte) => (Token::Byte(byte), next.saturating_add(1)),
    None => (Token::Never, next),
  }
}

/// The stars from `at`: one is [`Token::Run`]; two or more (any more are the same) are read by where they stand, as
/// `dowild` reads them with `WM_PATHNAME` (the module doc).
fn stars(pattern: &[u8], at: usize) -> (Token, usize) {
  let count = pattern.get(at..).map_or(0, |rest| {
    rest.iter().take_while(|&&byte| byte == b'*').count()
  });
  let end = at.saturating_add(count);
  if pattern.get(at.saturating_add(1)) != Some(&b'*') {
    return (Token::Run, end);
  }
  let starts_segment = at
    .checked_sub(1)
    .is_none_or(|before| pattern.get(before) == Some(&b'/'));
  if !starts_segment {
    return (Token::Run, end);
  }
  match (pattern.get(end), pattern.get(end.saturating_add(1))) {
    (None, _) => (Token::RunAll, end),
    (Some(b'/'), _) => (Token::Dirs, end.saturating_add(1)),
    (Some(b'\\'), Some(b'/')) => (Token::RunAll, end),
    _ => (Token::Run, end),
  }
}

/// One member of a bracket expression, read: the index of its last byte, and the byte a `-` after it may start a range
/// from (none after a range or a class, as `dowild` sets its `prev_ch` to 0 there).
struct Member {
  /// The index of the member's last byte.
  last: usize,
  /// The byte a following `-` ranges from.
  previous: Option<u8>,
}

/// The bracket expression whose `[` is at `open`, read as `dowild` reads one: its set, which never holds `/`, and where
/// the pattern resumes after its `]`; or [`Token::Never`] when it is cut short or names a class git does not know.
fn bracket(pattern: &[u8], open: usize) -> (Token, usize) {
  let mut at = open.saturating_add(1);
  let negated = matches!(pattern.get(at), Some(b'!' | b'^'));
  if negated {
    at = at.saturating_add(1);
  }
  let mut set = ByteSet::default();
  let mut previous = None;
  loop {
    let Some(member) = member(pattern, at, previous, &mut set) else {
      return (Token::Never, pattern.len());
    };
    previous = member.previous;
    at = member.last.saturating_add(1);
    match pattern.get(at) {
      Some(b']') => return (Token::Set(set.finished(negated)), at.saturating_add(1)),
      Some(_) => {}
      None => return (Token::Never, pattern.len()),
    }
  }
}

/// The member at `at`, its bytes added to `set`: an escaped byte, a range, a class, or one byte. `None` when the
/// pattern ends inside it or it names a class git does not know.
fn member(pattern: &[u8], at: usize, previous: Option<u8>, set: &mut ByteSet) -> Option<Member> {
  let &byte = pattern.get(at)?;
  let after = at.saturating_add(1);
  let ranges = pattern.get(after).is_some_and(|&next| next != b']');
  match (byte, previous) {
    (b'\\', _) => {
      let &member = pattern.get(after)?;
      set.insert(member);
      Some(Member {
        last: after,
        previous: Some(member),
      })
    }
    (b'-', Some(low)) if ranges => range(pattern, after, low, set),
    (b'[', _) if pattern.get(after) == Some(&b':') => class(pattern, at, set),
    _ => {
      set.insert(byte);
      Some(Member {
        last: at,
        previous: Some(byte),
      })
    }
  }
}

/// The range from `low` to the byte at `at` (escaped or not), both included; empty when `low` is past it.
fn range(pattern: &[u8], at: usize, low: u8, set: &mut ByteSet) -> Option<Member> {
  let (high, last) = match pattern.get(at)? {
    b'\\' => {
      let next = at.saturating_add(1);
      (*pattern.get(next)?, next)
    }
    &byte => (byte, at),
  };
  set.insert_range(low, high);
  Some(Member {
    last,
    previous: None,
  })
}

/// `[:` at `open` inside a bracket expression: a class `[:name:]` closed by the next `]`, or, when the bytes before
/// that `]` do not end in `:`, a plain `[` member (`dowild`: "Didn't find ":]", so treat like a normal set").
fn class(pattern: &[u8], open: usize, set: &mut ByteSet) -> Option<Member> {
  let name_start = open.saturating_add(2);
  let offset = pattern
    .get(name_start..)?
    .iter()
    .position(|&byte| byte == b']')?;
  let close = name_start.saturating_add(offset);
  let colon = close
    .checked_sub(1)
    .filter(|&colon| colon >= name_start && pattern.get(colon) == Some(&b':'));
  let Some(colon) = colon else {
    set.insert(b'[');
    return Some(Member {
      last: open,
      previous: Some(b'['),
    });
  };
  let class = Class::named(pattern.get(name_start..colon)?)?;
  set.insert_class(class);
  Some(Member {
    last: close,
    previous: None,
  })
}

/// A set of bytes, one bit each.
/// Shape: the 256 byte values in four 64-bit words.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ByteSet([u64; 4]);

/// Format: a byte's word in a [`ByteSet`] is its top two bits (four words of 64 bits hold 256 bytes).
const WORD_SHIFT: u32 = 6;
/// Format: a byte's bit in its word is its low six bits.
const BIT_MASK: u8 = 0x3f;

impl ByteSet {
  /// The word index and the bit of `byte`.
  fn place(byte: u8) -> (usize, u64) {
    (
      usize::from(byte.wrapping_shr(WORD_SHIFT)),
      1_u64.wrapping_shl(u32::from(byte & BIT_MASK)),
    )
  }

  /// Adds `byte`.
  fn insert(&mut self, byte: u8) {
    let (word, bit) = Self::place(byte);
    if let Some(word) = self.0.get_mut(word) {
      *word |= bit;
    }
  }

  /// Adds every byte from `low` to `high`, both included; nothing when `low` is past `high`.
  fn insert_range(&mut self, low: u8, high: u8) {
    for byte in low..=high {
      self.insert(byte);
    }
  }

  /// Adds every byte of `class`.
  fn insert_class(&mut self, class: Class) {
    for byte in u8::MIN..=u8::MAX {
      if class.holds(byte) {
        self.insert(byte);
      }
    }
  }

  /// Whether `byte` is in the set.
  fn contains(&self, byte: u8) -> bool {
    let (word, bit) = Self::place(byte);
    self.0.get(word).is_some_and(|word| word & bit != 0)
  }

  /// The set a bracket expression matches: its members, or every byte but them when `negated`, and never `/`
  /// (`dowild` refuses a `/` after the class whatever it held, under `WM_PATHNAME`).
  fn finished(self, negated: bool) -> Self {
    let mut set = if negated {
      Self(self.0.map(|word| !word))
    } else {
      self
    };
    let (word, bit) = Self::place(b'/');
    if let Some(word) = set.0.get_mut(word) {
      *word &= !bit;
    }
    set
  }
}

/// The twelve classes `dowild` names (`CC_EQ` in `wildmatch.c`), each over git's ASCII table (`ctype.c`,
/// `git-compat-util.h` at e1ec09b23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
  /// `[:alnum:]`.
  Alnum,
  /// `[:alpha:]`.
  Alpha,
  /// `[:blank:]`: space and tab.
  Blank,
  /// `[:cntrl:]`: 0 to 31, and 127.
  Cntrl,
  /// `[:digit:]`.
  Digit,
  /// `[:graph:]`: printable, not space.
  Graph,
  /// `[:lower:]`.
  Lower,
  /// `[:print:]`: space to tilde.
  Print,
  /// `[:punct:]`: ASCII punctuation (`ctype.c`'s `P`, `R`, `G` and `U` together).
  Punct,
  /// `[:space:]`: git's own, tab, newline, carriage return and space (no vertical tab, no form feed).
  Space,
  /// `[:upper:]`.
  Upper,
  /// `[:xdigit:]`.
  Xdigit,
}

impl Class {
  /// The class `name` names, or none for a name `dowild` refuses ("malformed [:class:] string").
  fn named(name: &[u8]) -> Option<Self> {
    Some(match name {
      b"alnum" => Self::Alnum,
      b"alpha" => Self::Alpha,
      b"blank" => Self::Blank,
      b"cntrl" => Self::Cntrl,
      b"digit" => Self::Digit,
      b"graph" => Self::Graph,
      b"lower" => Self::Lower,
      b"print" => Self::Print,
      b"punct" => Self::Punct,
      b"space" => Self::Space,
      b"upper" => Self::Upper,
      b"xdigit" => Self::Xdigit,
      _ => return None,
    })
  }

  /// Whether `byte` is in the class.
  fn holds(self, byte: u8) -> bool {
    match self {
      Self::Alnum => byte.is_ascii_alphanumeric(),
      Self::Alpha => byte.is_ascii_alphabetic(),
      Self::Blank => byte == b' ' || byte == b'\t',
      Self::Cntrl => byte.is_ascii_control(),
      Self::Digit => byte.is_ascii_digit(),
      Self::Graph => byte.is_ascii_graphic(),
      Self::Lower => byte.is_ascii_lowercase(),
      Self::Print => byte == b' ' || byte.is_ascii_graphic(),
      Self::Punct => byte.is_ascii_punctuation(),
      Self::Space => matches!(byte, b'\t' | b'\n' | b'\r' | b' '),
      Self::Upper => byte.is_ascii_uppercase(),
      Self::Xdigit => byte.is_ascii_hexdigit(),
    }
  }
}

#[cfg(test)]
pub(crate) mod reference;

#[cfg(test)]
mod tests {
  use proptest::prelude::*;

  use super::{Glob, reference};

  /// A-640 part 4: do compile an empty pattern, then patterns of at least one, three and seven tokens (a byte; a set
  /// and a star; a slash, `**/` and `?` besides). Expect: the empty one keeps nothing, and each other keeps room for
  /// every one of its tokens, so a caller charging what it keeps charges every token.
  #[test]
  fn a_pattern_keeps_room_for_each_of_its_tokens() {
    let token = u64::try_from(core::mem::size_of::<super::Token>()).unwrap();
    assert_eq!(Glob::new(b"").kept_bytes(), 0);
    for (pattern, tokens) in [(&b"a"[..], 1), (b"a[bc]*", 3), (b"a[bc]*/**/d?", 7)] {
      let kept = Glob::new(pattern).kept_bytes();
      assert!(kept >= tokens * token, "{pattern:?} keeps {kept} bytes");
    }
  }

  /// git's own vectors (`t/t3070-wildmatch.sh` at git e1ec09b23, 190 `match` lines): whether `wildmatch` with
  /// `WM_PATHNAME` matches each text with each pattern (the first column), with `t/helper/test-wildmatch.c`'s rule
  /// applied (an argument beginning `XXX/` loses its first three bytes). Made by reading every `match` line with the
  /// shell's quoting.
  const T3070: &[(bool, &[u8], &[u8])] = &[
    (true, br"foo", br"foo"),
    (false, br"foo", br"bar"),
    (true, br"", br""),
    (true, br"foo", br"???"),
    (false, br"foo", br"??"),
    (true, br"foo", br"*"),
    (true, br"foo", br"f*"),
    (false, br"foo", br"*f"),
    (true, br"foo", br"*foo*"),
    (true, br"foobar", br"*ob*a*r*"),
    (true, br"aaaaaaabababab", br"*ab"),
    (true, br"foo*", br"foo\*"),
    (false, br"foobar", br"foo\*bar"),
    (true, br"f\oo", br"f\\oo"),
    (false, br"foo\", br"foo\"),
    (true, br"ball", br"*[al]?"),
    (false, br"ten", br"[ten]"),
    (true, br"ten", br"**[!te]"),
    (false, br"ten", br"**[!ten]"),
    (true, br"ten", br"t[a-g]n"),
    (false, br"ten", br"t[!a-g]n"),
    (true, br"ton", br"t[!a-g]n"),
    (true, br"ton", br"t[^a-g]n"),
    (true, br"a]b", br"a[]]b"),
    (true, br"a-b", br"a[]-]b"),
    (true, br"a]b", br"a[]-]b"),
    (false, br"aab", br"a[]-]b"),
    (true, br"aab", br"a[]a-]b"),
    (true, br"]", br"]"),
    (false, br"foo/baz/bar", br"foo*bar"),
    (false, br"foo/baz/bar", br"foo**bar"),
    (true, br"foobazbar", br"foo**bar"),
    (true, br"foo/baz/bar", br"foo/**/bar"),
    (true, br"foo/baz/bar", br"foo/**/**/bar"),
    (true, br"foo/b/a/z/bar", br"foo/**/bar"),
    (true, br"foo/b/a/z/bar", br"foo/**/**/bar"),
    (true, br"foo/bar", br"foo/**/bar"),
    (true, br"foo/bar", br"foo/**/**/bar"),
    (false, br"foo/bar", br"foo?bar"),
    (false, br"foo/bar", br"foo[/]bar"),
    (false, br"foo/bar", br"foo[^a-z]bar"),
    (false, br"foo/bar", br"f[^eiu][^eiu][^eiu][^eiu][^eiu]r"),
    (true, br"foo-bar", br"f[^eiu][^eiu][^eiu][^eiu][^eiu]r"),
    (true, br"foo", br"**/foo"),
    (true, br"/foo", br"**/foo"),
    (true, br"bar/baz/foo", br"**/foo"),
    (false, br"bar/baz/foo", br"*/foo"),
    (false, br"foo/bar/baz", br"**/bar*"),
    (true, br"deep/foo/bar/baz", br"**/bar/*"),
    (false, br"deep/foo/bar/baz/", br"**/bar/*"),
    (true, br"deep/foo/bar/baz/", br"**/bar/**"),
    (false, br"deep/foo/bar", br"**/bar/*"),
    (true, br"deep/foo/bar/", br"**/bar/**"),
    (false, br"foo/bar/baz", br"**/bar**"),
    (true, br"foo/bar/baz/x", br"*/bar/**"),
    (false, br"deep/foo/bar/baz/x", br"*/bar/**"),
    (true, br"deep/foo/bar/baz/x", br"**/bar/*/*"),
    (false, br"acrt", br"a[c-c]st"),
    (true, br"acrt", br"a[c-c]rt"),
    (false, br"]", br"[!]-]"),
    (true, br"a", br"[!]-]"),
    (false, br"", br"\"),
    (false, br"\", br"\"),
    (false, br"/\", br"*/\"),
    (true, br"/\", br"*/\\"),
    (true, br"foo", br"foo"),
    (true, br"@foo", br"@foo"),
    (false, br"foo", br"@foo"),
    (true, br"[ab]", br"\[ab]"),
    (true, br"[ab]", br"[[]ab]"),
    (true, br"[ab]", br"[[:]ab]"),
    (false, br"[ab]", br"[[::]ab]"),
    (true, br"[ab]", br"[[:digit]ab]"),
    (true, br"[ab]", br"[\[:]ab]"),
    (true, br"?a?b", br"\??\?b"),
    (true, br"abc", br"\a\b\c"),
    (false, br"foo", br""),
    (true, br"foo/bar/baz/to", br"**/t[o]"),
    (true, br"a1B", br"[[:alpha:]][[:digit:]][[:upper:]]"),
    (false, br"a", br"[[:digit:][:upper:][:space:]]"),
    (true, br"A", br"[[:digit:][:upper:][:space:]]"),
    (true, br"1", br"[[:digit:][:upper:][:space:]]"),
    (false, br"1", br"[[:digit:][:upper:][:spaci:]]"),
    (true, br" ", br"[[:digit:][:upper:][:space:]]"),
    (false, br".", br"[[:digit:][:upper:][:space:]]"),
    (true, br".", br"[[:digit:][:punct:][:space:]]"),
    (true, br"5", br"[[:xdigit:]]"),
    (true, br"f", br"[[:xdigit:]]"),
    (true, br"D", br"[[:xdigit:]]"),
    (true, br"_", br"[[:alnum:][:alpha:][:blank:][:cntrl:][:digit:][:graph:][:lower:][:print:][:punct:][:space:][:upper:][:xdigit:]]"),
    (true, br".", br"[^[:alnum:][:alpha:][:blank:][:cntrl:][:digit:][:lower:][:space:][:upper:][:xdigit:]]"),
    (true, br"5", br"[a-c[:digit:]x-z]"),
    (true, br"b", br"[a-c[:digit:]x-z]"),
    (true, br"y", br"[a-c[:digit:]x-z]"),
    (false, br"q", br"[a-c[:digit:]x-z]"),
    (true, br"]", br"[\\-^]"),
    (false, br"[", br"[\\-^]"),
    (true, br"-", br"[\-_]"),
    (true, br"]", br"[\]]"),
    (false, br"\]", br"[\]]"),
    (false, br"\", br"[\]]"),
    (false, br"ab", br"a[]b"),
    (false, br"a[]b", br"a[]b"),
    (false, br"ab[", br"ab["),
    (false, br"ab", br"[!"),
    (false, br"ab", br"[-"),
    (true, br"-", br"[-]"),
    (false, br"-", br"[a-"),
    (false, br"-", br"[!a-"),
    (true, br"-", br"[--A]"),
    (true, br"5", br"[--A]"),
    (true, br" ", br"[ --]"),
    (true, br"$", br"[ --]"),
    (true, br"-", br"[ --]"),
    (false, br"0", br"[ --]"),
    (true, br"-", br"[---]"),
    (true, br"-", br"[------]"),
    (false, br"j", br"[a-e-n]"),
    (true, br"-", br"[a-e-n]"),
    (true, br"a", br"[!------]"),
    (false, br"[", br"[]-a]"),
    (true, br"^", br"[]-a]"),
    (false, br"^", br"[!]-a]"),
    (true, br"[", br"[!]-a]"),
    (true, br"^", br"[a^bc]"),
    (true, br"-b]", br"[a-]b]"),
    (false, br"\", br"[\]"),
    (true, br"\", br"[\\]"),
    (false, br"\", br"[!\\]"),
    (true, br"G", br"[A-\\]"),
    (false, br"aaabbb", br"b*a"),
    (false, br"aabcaa", br"*ba*"),
    (true, br",", br"[,]"),
    (true, br",", br"[\\,]"),
    (true, br"\", br"[\\,]"),
    (true, br"-", br"[,-.]"),
    (false, br"+", br"[,-.]"),
    (false, br"-.]", br"[,-.]"),
    (true, br"2", br"[\1-\3]"),
    (true, br"3", br"[\1-\3]"),
    (false, br"4", br"[\1-\3]"),
    (true, br"\", br"[[-\]]"),
    (true, br"[", br"[[-\]]"),
    (true, br"]", br"[[-\]]"),
    (false, br"-", br"[[-\]]"),
    (true, br"-adobe-courier-bold-o-normal--12-120-75-75-m-70-iso8859-1", br"-*-*-*-*-*-*-12-*-*-*-m-*-*-*"),
    (false, br"-adobe-courier-bold-o-normal--12-120-75-75-X-70-iso8859-1", br"-*-*-*-*-*-*-12-*-*-*-m-*-*-*"),
    (false, br"-adobe-courier-bold-o-normal--12-120-75-75-/-70-iso8859-1", br"-*-*-*-*-*-*-12-*-*-*-m-*-*-*"),
    (true, br"/adobe/courier/bold/o/normal//12/120/75/75/m/70/iso8859/1", br"/*/*/*/*/*/*/12/*/*/*/m/*/*/*"),
    (false, br"/adobe/courier/bold/o/normal//12/120/75/75/X/70/iso8859/1", br"/*/*/*/*/*/*/12/*/*/*/m/*/*/*"),
    (true, br"abcd/abcdefg/abcdefghijk/abcdefghijklmnop.txt", br"**/*a*b*g*n*t"),
    (false, br"abcd/abcdefg/abcdefghijk/abcdefghijklmnop.txtz", br"**/*a*b*g*n*t"),
    (false, br"foo", br"*/*/*"),
    (false, br"foo/bar", br"*/*/*"),
    (true, br"foo/bba/arr", br"*/*/*"),
    (false, br"foo/bb/aa/rr", br"*/*/*"),
    (true, br"foo/bb/aa/rr", br"**/**/**"),
    (true, br"abcXdefXghi", br"*X*i"),
    (false, br"ab/cXd/efXg/hi", br"*X*i"),
    (true, br"ab/cXd/efXg/hi", br"*/*X*/*/*i"),
    (true, br"ab/cXd/efXg/hi", br"**/*X*/**/*i"),
    (false, br"foo", br"fo"),
    (true, br"foo/bar", br"foo/bar"),
    (true, br"foo/bar", br"foo/*"),
    (false, br"foo/bba/arr", br"foo/*"),
    (true, br"foo/bba/arr", br"foo/**"),
    (false, br"foo/bba/arr", br"foo*"),
    (false, br"foo/bba/arr", br"foo**"),
    (false, br"foo/bba/arr", br"foo/*arr"),
    (false, br"foo/bba/arr", br"foo/**arr"),
    (false, br"foo/bba/arr", br"foo/*z"),
    (false, br"foo/bba/arr", br"foo/**z"),
    (false, br"foo/bar", br"foo?bar"),
    (false, br"foo/bar", br"foo[/]bar"),
    (false, br"foo/bar", br"foo[^a-z]bar"),
    (false, br"ab/cXd/efXg/hi", br"*Xg*i"),
    (false, br"a", br"[A-Z]"),
    (true, br"A", br"[A-Z]"),
    (false, br"A", br"[a-z]"),
    (true, br"a", br"[a-z]"),
    (false, br"a", br"[[:upper:]]"),
    (true, br"A", br"[[:upper:]]"),
    (false, br"A", br"[[:lower:]]"),
    (true, br"a", br"[[:lower:]]"),
    (false, br"A", br"[B-Za]"),
    (true, br"a", br"[B-Za]"),
    (false, br"A", br"[B-a]"),
    (true, br"a", br"[B-a]"),
    (false, br"z", br"[Z-y]"),
    (true, br"Z", br"[Z-y]"),
  ];

  /// A-597 (golden vectors). Do: match every text of git's t3070 against its pattern, with the compiled matcher and with
  /// the transcription of git's `dowild`. Expect: each answers git's own `WM_PATHNAME` column for all 190, so both say
  /// what git says, quirks included (`[]-]`, `[[:]ab]`, `**/` matching no directory, a lone trailing `\`).
  #[test]
  fn every_vector_of_gits_own_wildmatch_test_is_answered_as_git_answers_it() {
    assert_eq!(T3070.len(), 190, "t3070's 190 match lines");
    for &(expected, text, pattern) in T3070 {
      assert_eq!(
        Glob::new(pattern).matches(text),
        expected,
        "compiled: {:?} against {:?}",
        String::from_utf8_lossy(pattern),
        String::from_utf8_lossy(text),
      );
      assert_eq!(
        reference::wildmatch(pattern, text),
        expected,
        "dowild: {:?} against {:?}",
        String::from_utf8_lossy(pattern),
        String::from_utf8_lossy(text),
      );
    }
  }

  /// Shape: star groups in the long pattern below, as many as its text has bytes.
  const LONG: usize = 1024;

  /// A-597 (bounded work). Do: match a pattern of 1,024 star groups (`a*` repeated) against a 1,024-byte text ending in
  /// a byte it cannot match, then against one it can. Expect: no match, then a match, each reached by one pass a token
  /// with no recursion, where `dowild` recurses once a star group.
  #[test]
  fn a_long_starry_pattern_is_answered_without_recursion() {
    let pattern = b"a*".repeat(LONG);
    let mut text = b"a".repeat(LONG - 1);
    text.push(b'b');
    assert!(!Glob::new(&pattern).matches(&text));
    assert!(Glob::new(&pattern).matches(&b"a".repeat(LONG)));
  }

  /// The pattern pieces generated patterns are made of: every special git reads, each class form, and plain bytes.
  fn piece() -> impl Strategy<Value = &'static [u8]> {
    prop::sample::select(vec![
      &b"a"[..],
      &b"b"[..],
      &b"/"[..],
      &b"*"[..],
      &b"**"[..],
      &b"**/"[..],
      &b"/**"[..],
      &b"?"[..],
      &b"["[..],
      &b"]"[..],
      &b"!"[..],
      &b"^"[..],
      &b"-"[..],
      &b"\\"[..],
      &b":"[..],
      &b"[:alpha:]"[..],
      &b"[:digit:]"[..],
      &b"[:bogus:]"[..],
      &b"[a-b]"[..],
      &b"[!a]"[..],
    ])
  }

  /// The bytes generated texts are made of.
  fn text_byte() -> impl Strategy<Value = u8> {
    prop::sample::select(vec![
      b'a', b'b', b'/', b'1', b'-', b']', b'[', b'\\', b':', b'!',
    ])
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
      cases: slates_test_seeds::cases(if cfg!(miri) { 16 } else { 4096 }),
      ..ProptestConfig::default()
    }))]

    /// A-597 (the oracle). Do: match generated texts against generated patterns made of every special git reads.
    /// Expect: the compiled matcher answers as the transcription of git's `dowild` answers, on every case.
    #[test]
    fn the_compiled_matcher_answers_as_gits_dowild_on_every_generated_case(
      pieces in prop::collection::vec(piece(), 0..10),
      text in prop::collection::vec(text_byte(), 0..12),
    ) {
      let pattern: Vec<u8> = pieces.concat();
      prop_assert_eq!(
        Glob::new(&pattern).matches(&text),
        reference::wildmatch(&pattern, &text),
        "{:?} against {:?}",
        String::from_utf8_lossy(&pattern),
        String::from_utf8_lossy(&text),
      );
    }
  }
}
