//! A property list as content (A-628): the bytes FSFS keeps a node's property list in, `svn_hash_write2`'s form
//! (`hash.c` 280-324): for each property in name order a `K` entry with the name and a `V` entry with the value, each its
//! length on a line and then its bytes and a line feed, and `END` on a line last. FSFS writes that form for every list
//! it keeps, an empty one included (`transaction.c` `svn_fs_fs__set_proplist`, which has no case for an empty list),
//! and shares one representation among equal lists (`write_container_rep` with rep-sharing allowed, 3257-3277), so a
//! list's content identity, the BLAKE3 of these bytes, is equal exactly when FSFS's representations are shared.

use super::records::Properties;

/// The property `svn:mergeinfo`, whose presence FSFS counts and whose changes its changed paths flag.
/// Format: `SVN_PROP_MERGEINFO` (`svn_props.h`).
pub(super) const MERGEINFO: &[u8] = b"svn:mergeinfo";

/// The line that ends a property list FSFS keeps, its line feed included.
/// Format: `SVN_HASH_TERMINATOR` "END" (`svn_hash.h`), written by `svn_hash_write2` with a line feed (`hash.c` 319).
const TERMINATOR: &[u8] = b"END\n";

/// What begins a name's entry.
/// Format: `hash.c` 288 writes `K <length>\n`.
const NAME_ENTRY: &[u8] = b"K ";

/// What begins a value's entry.
/// Format: `hash.c` 290 writes `V <length>\n`.
const VALUE_ENTRY: &[u8] = b"V ";

/// The byte that ends every line of the form.
/// Format: `hash.c` 288-293.
const LINE_FEED: u8 = b'\n';

/// The bytes of `properties` as FSFS keeps them.
pub(crate) fn encode(properties: &Properties) -> Vec<u8> {
  let mut bytes = Vec::new();
  for (name, value) in properties {
    entry(&mut bytes, NAME_ENTRY, name);
    entry(&mut bytes, VALUE_ENTRY, value);
  }
  bytes.extend_from_slice(TERMINATOR);
  bytes
}

/// Appends one entry: its prefix, its length on a line, its bytes and a line feed.
fn entry(bytes: &mut Vec<u8>, prefix: &[u8], data: &[u8]) {
  bytes.extend_from_slice(prefix);
  bytes.extend_from_slice(data.len().to_string().as_bytes());
  bytes.push(LINE_FEED);
  bytes.extend_from_slice(data);
  bytes.push(LINE_FEED);
}

/// The list `bytes` hold, if they are a list [`encode`] wrote: entries in strictly increasing name order, every
/// length decimal digits, nothing after `END`.
pub(crate) fn decode(bytes: &[u8]) -> Option<Properties> {
  Some(
    entries(bytes)?
      .into_iter()
      .map(|(name, value)| (name.to_vec(), value.to_vec()))
      .collect(),
  )
}

/// The entries of the list `bytes` hold, in name order, each borrowed from `bytes`, if they are a list [`encode`]
/// wrote ([`decode`]'s rules).
pub(crate) fn entries(bytes: &[u8]) -> Option<Vec<(&[u8], &[u8])>> {
  let mut entries: Vec<(&[u8], &[u8])> = Vec::new();
  let mut rest = bytes;
  loop {
    if rest == TERMINATOR {
      return Some(entries);
    }
    let (name, after_name) = take_entry(rest, NAME_ENTRY)?;
    let (value, after_value) = take_entry(after_name, VALUE_ENTRY)?;
    let in_order = entries.last().is_none_or(|(last, _)| *last < name);
    if !in_order {
      return None;
    }
    entries.push((name, value));
    rest = after_value;
  }
}

/// The data of the entry at the start of `bytes` with `prefix`, and what follows it.
fn take_entry<'a>(bytes: &'a [u8], prefix: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
  let after_prefix = bytes.strip_prefix(prefix)?;
  let line_end = after_prefix.iter().position(|&byte| byte == LINE_FEED)?;
  let digits = after_prefix.get(..line_end)?;
  let length = parse_length(digits)?;
  let data_start = line_end.checked_add(1)?;
  let data_end = data_start.checked_add(length)?;
  let data = after_prefix.get(data_start..data_end)?;
  let rest = after_prefix.get(data_end..)?.strip_prefix(&[LINE_FEED])?;
  Some((data, rest))
}

/// A length written as decimal digits with no sign and no leading zero but `0` itself.
fn parse_length(digits: &[u8]) -> Option<usize> {
  let canonical = !digits.is_empty()
    && digits.iter().all(u8::is_ascii_digit)
    && (digits.len() == 1 || digits.first() != Some(&b'0'));
  if !canonical {
    return None;
  }
  std::str::from_utf8(digits).ok()?.parse().ok()
}

/// The value of `svn:mergeinfo` in `properties`, if it holds one.
pub(super) fn mergeinfo(properties: &Properties) -> Option<&[u8]> {
  properties.get(MERGEINFO).map(Vec::as_slice)
}
