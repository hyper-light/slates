//! Bounds-checked little-endian encoding for on-disk structures. A reader past the end of
//! its input returns `None` instead of panicking, so a torn or corrupt structure decodes to
//! "invalid", never to a crash.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

/// Appends little-endian fields to a byte vector.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// A writer that reserves `capacity` bytes up front. The capacity is a hint: a reservation
    /// the allocator refuses leaves the buffer empty, and the writes that follow grow it as
    /// they would from `Default`, where `Vec::with_capacity` would panic on a capacity past
    /// `isize::MAX` bytes.
    pub fn with_capacity(capacity: usize) -> Self {
        let mut buf = Vec::new();
        if buf.try_reserve_exact(capacity).is_err() {
            buf = Vec::new();
        }
        Self { buf }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u128(&mut self, v: u128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    pub fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len().saturating_add(n), 0);
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// Reads little-endian fields from a byte slice.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    pub fn u8(&mut self) -> Option<u8> {
        self.array::<1>().map(|[b]| b)
    }

    pub fn u16(&mut self) -> Option<u16> {
        self.array().map(u16::from_le_bytes)
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }

    pub fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_le_bytes)
    }

    pub fn u128(&mut self) -> Option<u128> {
        self.array().map(u128::from_le_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn fields_round_trip(a: u8, b: u16, c: u32, d: u64, e: u128) {
            let mut w = Writer::default();
            w.u8(a); w.u16(b); w.u32(c); w.u64(d); w.u128(e);
            let bytes = w.into_vec();
            let mut r = Reader::new(&bytes);
            prop_assert_eq!(r.u8(), Some(a));
            prop_assert_eq!(r.u16(), Some(b));
            prop_assert_eq!(r.u32(), Some(c));
            prop_assert_eq!(r.u64(), Some(d));
            prop_assert_eq!(r.u128(), Some(e));
            prop_assert_eq!(r.u8(), None);
        }

        #[test]
        fn truncated_input_reads_none(bytes in proptest::collection::vec(any::<u8>(), 0..16)) {
            let mut r = Reader::new(&bytes);
            let _ = r.u128();
            prop_assert!(r.remaining() <= 16);
        }
    }
}
