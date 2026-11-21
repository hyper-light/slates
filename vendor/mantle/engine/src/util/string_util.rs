//! Fixed-width digit strings: `PutBaseChars` and `ToBaseCharsString` of RocksDB's
//! `util/string_util.h`, which spell the SST unique ID in base 36 [R table/unique_id.cc:23],
//! a UUID in base 16 [R env/env.cc:882-890] and compression names [R util/compression.cc:67].

const UPPER_DIGITS: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const LOWER_DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// `PutBaseChars<base>` [R util/string_util.h:31-41]: the low `n` base-`base` digits of `v`,
/// most significant first, appended to `dst`. A `base` outside 2..=36 writes nothing.
pub fn put_base_chars(dst: &mut String, base: u64, n: usize, v: u64, uppercase: bool) {
    if !(2..=36).contains(&base) {
        return;
    }
    let digits = if uppercase {
        UPPER_DIGITS
    } else {
        LOWER_DIGITS
    };
    let mut out = vec![b'0'; n];
    let mut rest = v;
    for slot in out.iter_mut().rev() {
        // `base` is at least 2, checked above.
        let digit = usize::try_from(rest.checked_rem(base).unwrap_or(0)).unwrap_or(0);
        *slot = digits.get(digit).copied().unwrap_or(b'0');
        rest = rest.checked_div(base).unwrap_or(0);
    }
    dst.extend(out.into_iter().map(char::from));
}

/// `ToBaseCharsString<base>` [R util/string_util.h:43-51].
pub fn to_base_chars_string(base: u64, n: usize, v: u64, uppercase: bool) -> String {
    let mut s = String::new();
    put_base_chars(&mut s, base, n, v, uppercase);
    s
}
