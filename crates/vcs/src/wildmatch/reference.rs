//! git's `dowild` (`wildmatch.c` at git e1ec09b23), transcribed line for line with `WM_PATHNAME` set and
//! `WM_CASEFOLD` clear, over git's own ctype table (`ctype.c`, `git-compat-util.h`): the oracle the compiled
//! matcher is held to. A NUL ends a string here as it does there, so it is fed NUL-free strings only.

/// `WM_MATCH`.
const MATCH: i8 = 0;
/// `WM_NOMATCH`.
const NOMATCH: i8 = 1;
/// `WM_ABORT_ALL`.
const ABORT_ALL: i8 = -1;
/// `WM_ABORT_TO_STARSTAR`.
const ABORT_TO_STARSTAR: i8 = -2;

/// Format: `ctype.c`'s `GIT_SPACE`.
const S: u8 = 0x01;
/// Format: `GIT_DIGIT`.
const D: u8 = 0x02;
/// Format: `GIT_ALPHA`.
const A: u8 = 0x04;
/// Format: `GIT_GLOB_SPECIAL` (`*`, `?`, `[`, `\`).
const G: u8 = 0x08;
/// Format: `GIT_REGEX_SPECIAL`.
const R: u8 = 0x10;
/// Format: `GIT_PATHSPEC_MAGIC`.
const P: u8 = 0x20;
/// Format: `GIT_CNTRL`.
const X: u8 = 0x40;
/// Format: `GIT_PUNCT`.
const U: u8 = 0x80;
/// Format: `GIT_CNTRL | GIT_SPACE`.
const Z: u8 = X | S;

/// `sane_ctype` for 0 to 127, as `ctype.c` lists it (nothing from 128 up).
#[rustfmt::skip]
const SANE_CTYPE: [u8; 128] = [
  X, X, X, X, X, X, X, X, X, Z, Z, X, X, Z, X, X,
  X, X, X, X, X, X, X, X, X, X, X, X, X, X, X, X,
  S, P, P, P, R, P, P, P, R, R, G, R, P, P, R, P,
  D, D, D, D, D, D, D, D, D, D, P, P, P, P, P, G,
  P, A, A, A, A, A, A, A, A, A, A, A, A, A, A, A,
  A, A, A, A, A, A, A, A, A, A, A, G, G, U, R, P,
  P, A, A, A, A, A, A, A, A, A, A, A, A, A, A, A,
  A, A, A, A, A, A, A, A, A, A, A, R, R, U, P, X,
];

/// `sane_istest`.
fn sane(c: u8, mask: u8) -> bool {
  (c as usize) < SANE_CTYPE.len() && SANE_CTYPE[c as usize] & mask != 0
}

/// `is_glob_special`.
fn is_glob_special(c: u8) -> bool {
  sane(c, G)
}

/// The `CC_EQ` chain: whether `t_ch` is in the class `name`, or `None` for "malformed [:class:] string".
fn class_holds(name: &[u8], c: u8) -> Option<bool> {
  let isalpha = sane(c, A);
  // Format: the C locale's isprint is 0x20 to 0x7e; ASCII's case bit is 0x20 (`dowild` uses the C locale).
  let isprint = (0x20..=0x7e).contains(&c);
  let isspace = sane(c, S);
  Some(match name {
    b"alnum" => sane(c, A | D),
    b"alpha" => isalpha,
    b"blank" => c == b' ' || c == b'\t',
    b"cntrl" => sane(c, X),
    b"digit" => sane(c, D),
    b"graph" => isprint && !isspace,
    // Format: a lower-case ASCII letter has the case bit 0x20 set, an upper-case one clear.
    b"lower" => isalpha && c & 0x20 != 0,
    b"print" => isprint,
    b"punct" => sane(c, P | R | G | U),
    b"space" => isspace,
    // Format: ASCII's case bit, clear in an upper-case letter.
    b"upper" => isalpha && c & 0x20 == 0,
    b"xdigit" => c.is_ascii_hexdigit(),
    _ => return None,
  })
}

/// `*p` past the end is the terminating NUL.
fn at(s: &[u8], i: usize) -> u8 {
  s.get(i).copied().unwrap_or(0)
}

/// `wildmatch(pattern, text, WM_PATHNAME) == WM_MATCH`.
pub(crate) fn wildmatch(pattern: &[u8], text: &[u8]) -> bool {
  dowild(pattern, 0, text, 0) == MATCH
}

/// `dowild`, with `pattern` (its start in `pat`) as `start` and the text from `t`. Kept in `dowild`'s own shape, line
/// for line, so it reads against git's source; hence its complexity.
#[allow(clippy::cognitive_complexity)]
fn dowild(pat: &[u8], start: usize, text: &[u8], mut t: usize) -> i8 {
  let mut p = start;
  loop {
    let mut p_ch = at(pat, p);
    if p_ch == 0 {
      break;
    }
    let t_ch = at(text, t);
    if t_ch == 0 && p_ch != b'*' {
      return ABORT_ALL;
    }
    match p_ch {
      b'\\' => {
        p += 1;
        p_ch = at(pat, p);
        if t_ch != p_ch {
          return NOMATCH;
        }
      }
      b'?' => {
        if t_ch == b'/' {
          return NOMATCH;
        }
      }
      b'*' => {
        if let Some(code) = star(pat, start, &mut p, text, &mut t) {
          return code;
        }
      }
      b'[' => {
        if let Some(code) = bracket(pat, &mut p, t_ch) {
          return code;
        }
      }
      _ => {
        if t_ch != p_ch {
          return NOMATCH;
        }
      }
    }
    p += 1;
    t += 1;
  }
  if at(text, t) != 0 { NOMATCH } else { MATCH }
}

/// `case '*'`: `Some` when it returns, `None` for its `break` (one star before a slash, `t` left on the text's slash
/// for the loop's step past both). In `dowild`'s shape, line for line.
#[allow(clippy::cognitive_complexity)]
fn star(pat: &[u8], start: usize, p: &mut usize, text: &[u8], t: &mut usize) -> Option<i8> {
  let match_slash;
  *p += 1;
  if at(pat, *p) == b'*' {
    let prev_p = *p;
    loop {
      *p += 1;
      if at(pat, *p) != b'*' {
        break;
      }
    }
    let after = at(pat, *p);
    if (prev_p - start < 2 || at(pat, prev_p - 2) == b'/')
      && (after == 0 || after == b'/' || (after == b'\\' && at(pat, *p + 1) == b'/'))
    {
      if after == b'/' && dowild(pat, *p + 1, text, *t) == MATCH {
        return Some(MATCH);
      }
      match_slash = true;
    } else {
      match_slash = false;
    }
  } else {
    match_slash = false;
  }
  if at(pat, *p) == 0 {
    if !match_slash && text[*t..].contains(&b'/') {
      return Some(ABORT_TO_STARSTAR);
    }
    return Some(MATCH);
  } else if !match_slash && at(pat, *p) == b'/' {
    return match text[*t..].iter().position(|&c| c == b'/') {
      None => Some(ABORT_ALL),
      Some(offset) => {
        *t += offset;
        None
      }
    };
  }
  let mut t_ch = at(text, *t);
  loop {
    if t_ch == 0 {
      break;
    }
    if !is_glob_special(at(pat, *p)) {
      let p_ch = at(pat, *p);
      loop {
        t_ch = at(text, *t);
        if t_ch == 0 || !(match_slash || t_ch != b'/') {
          break;
        }
        if t_ch == p_ch {
          break;
        }
        *t += 1;
      }
      if t_ch != p_ch {
        return Some(if match_slash {
          ABORT_ALL
        } else {
          ABORT_TO_STARSTAR
        });
      }
    }
    let matched = dowild(pat, *p, text, *t);
    if matched != NOMATCH {
      if !match_slash || matched != ABORT_TO_STARSTAR {
        return Some(matched);
      }
    } else if !match_slash && t_ch == b'/' {
      return Some(ABORT_TO_STARSTAR);
    }
    *t += 1;
    t_ch = at(text, *t);
  }
  Some(ABORT_ALL)
}

/// `case '['`: `Some` when it returns, `None` when the byte is in the class (`p` on its closing `]`). In `dowild`'s
/// shape, line for line.
#[allow(clippy::cognitive_complexity)]
fn bracket(pat: &[u8], p: &mut usize, t_ch: u8) -> Option<i8> {
  *p += 1;
  let mut p_ch = at(pat, *p);
  if p_ch == b'^' {
    p_ch = b'!';
  }
  let negated = p_ch == b'!';
  if negated {
    *p += 1;
    p_ch = at(pat, *p);
  }
  let mut prev_ch: u8 = 0;
  let mut matched = false;
  loop {
    if p_ch == 0 {
      return Some(ABORT_ALL);
    }
    if p_ch == b'\\' {
      *p += 1;
      p_ch = at(pat, *p);
      if p_ch == 0 {
        return Some(ABORT_ALL);
      }
      if t_ch == p_ch {
        matched = true;
      }
    } else if p_ch == b'-' && prev_ch != 0 && at(pat, *p + 1) != 0 && at(pat, *p + 1) != b']' {
      *p += 1;
      p_ch = at(pat, *p);
      if p_ch == b'\\' {
        *p += 1;
        p_ch = at(pat, *p);
        if p_ch == 0 {
          return Some(ABORT_ALL);
        }
      }
      if t_ch <= p_ch && t_ch >= prev_ch {
        matched = true;
      }
      p_ch = 0;
    } else if p_ch == b'[' && at(pat, *p + 1) == b':' {
      *p += 2;
      let s = *p;
      while at(pat, *p) != 0 && at(pat, *p) != b']' {
        *p += 1;
      }
      p_ch = at(pat, *p);
      if p_ch == 0 {
        return Some(ABORT_ALL);
      }
      if *p == s || at(pat, *p - 1) != b':' {
        *p = s - 2;
        p_ch = b'[';
        if t_ch == p_ch {
          matched = true;
        }
      } else {
        match class_holds(&pat[s..*p - 1], t_ch) {
          None => return Some(ABORT_ALL),
          Some(true) => matched = true,
          Some(false) => {}
        }
        p_ch = 0;
      }
    } else if t_ch == p_ch {
      matched = true;
    }
    prev_ch = p_ch;
    *p += 1;
    p_ch = at(pat, *p);
    if p_ch == b']' {
      break;
    }
  }
  if matched == negated || t_ch == b'/' {
    return Some(NOMATCH);
  }
  None
}
