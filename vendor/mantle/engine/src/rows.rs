//! A page of rows a scan returns (docs/design/engine-structure.md §5, step E6): every key and
//! value back to back in one buffer, with each row's ends, so a page that is cleared and filled
//! again allocates nothing once it has grown to its largest.

/// Rows in key order: each a key and its value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rows {
    bytes: Vec<u8>,
    /// Each row's key end and value end in `bytes`.
    ends: Vec<(usize, usize)>,
}

impl Rows {
    /// No rows.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops every row, keeping the buffers.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.ends.clear();
    }

    /// The rows held.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether no row is held.
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Keeps the first `len` rows, dropping the rest, the buffers kept.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.ends.len() {
            return;
        }
        let end = len
            .checked_sub(1)
            .and_then(|last| self.ends.get(last))
            .map_or(0, |&(_, end)| end);
        self.ends.truncate(len);
        self.bytes.truncate(end);
    }

    /// Appends a row.
    pub fn push(&mut self, key: &[u8], value: &[u8]) {
        self.bytes.extend_from_slice(key);
        let key_end = self.bytes.len();
        self.bytes.extend_from_slice(value);
        self.ends.push((key_end, self.bytes.len()));
    }

    /// Row `i`: its key and value.
    pub fn get(&self, i: usize) -> Option<(&[u8], &[u8])> {
        let start = match i.checked_sub(1) {
            Some(before) => self.ends.get(before)?.1,
            None => 0,
        };
        let &(key_end, end) = self.ends.get(i)?;
        Some((
            self.bytes.get(start..key_end)?,
            self.bytes.get(key_end..end)?,
        ))
    }

    /// The rows in order.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        (0..self.len()).filter_map(|i| self.get(i))
    }

    /// Moves every row of `other` after these, leaving `other` empty with its buffers kept.
    pub fn append(&mut self, other: &mut Self) {
        let base = self.bytes.len();
        self.bytes.extend_from_slice(&other.bytes);
        self.ends.extend(
            other
                .ends
                .iter()
                .map(|&(k, v)| (k.saturating_add(base), v.saturating_add(base))),
        );
        other.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_read_back_as_pushed_and_append_in_order() {
        let mut a = Rows::new();
        a.push(b"a", b"1");
        a.push(b"", b"");
        a.push(b"ccc", b"three");
        let mut b = Rows::new();
        b.push(b"d", b"");
        a.append(&mut b);
        assert!(b.is_empty());
        let got: Vec<_> = a.iter().collect();
        assert_eq!(
            got,
            vec![
                (&b"a"[..], &b"1"[..]),
                (&b""[..], &b""[..]),
                (&b"ccc"[..], &b"three"[..]),
                (&b"d"[..], &b""[..]),
            ]
        );
        assert_eq!(a.get(4), None);
        a.clear();
        assert!(a.is_empty() && a.get(0).is_none());
    }
}
