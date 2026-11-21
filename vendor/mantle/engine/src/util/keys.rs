//! Comparisons of keys, eight bytes at a time. A key is a byte string ordered as slices order
//! (bytewise, a prefix before its extensions); the trunk, its branches and the memtables compare
//! keys on every entry they move. A byte loop compares one byte a step, and a slice comparison
//! calls `memcmp` out of line; here two keys are read as words, their first difference found
//! from the words' exclusive or, and the comparison stays inline.

/// The length of the prefix `a` and `b` share. Words are read little-endian, so the first byte
/// that differs is the exclusive or's lowest set byte.
#[inline]
pub fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let mut at = 0usize;
    while let (Some(x), Some(y)) = (
        a.get(at..).and_then(<[u8]>::first_chunk::<8>),
        b.get(at..).and_then(<[u8]>::first_chunk::<8>),
    ) {
        let diff = u64::from_le_bytes(*x) ^ u64::from_le_bytes(*y);
        if diff != 0 {
            return at.saturating_add(usize::try_from(diff.trailing_zeros() / 8).unwrap_or(0));
        }
        at = at.saturating_add(8);
    }
    // Fewer than eight bytes left in the shorter key. When it has eight in all, the word that
    // ends it overlaps bytes already found equal, so its first difference is one past them.
    let shorter = a.len().min(b.len());
    let last = shorter.checked_sub(8).and_then(|from| {
        let x = a.get(from..shorter)?.first_chunk::<8>()?;
        let y = b.get(from..shorter)?.first_chunk::<8>()?;
        let diff = u64::from_le_bytes(*x) ^ u64::from_le_bytes(*y);
        Some(if diff == 0 {
            shorter
        } else {
            from.saturating_add(usize::try_from(diff.trailing_zeros() / 8).unwrap_or(0))
        })
    });
    match last {
        Some(at) => at,
        None => {
            let (a, b) = (a.get(at..).unwrap_or(&[]), b.get(at..).unwrap_or(&[]));
            at.saturating_add(a.iter().zip(b).take_while(|(x, y)| x == y).count())
        }
    }
}

/// Whether `a` orders below `b`: past their common prefix, a smaller byte, or `a` ending there
/// while `b` goes on.
#[inline]
pub fn less(a: &[u8], b: &[u8]) -> bool {
    let at = common_prefix(a, b);
    match (a.get(at), b.get(at)) {
        (Some(x), Some(y)) => x < y,
        (None, Some(_)) => true,
        _ => false,
    }
}

/// Whether `a` and `b` are the same key.
#[inline]
pub fn equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && common_prefix(a, b) == a.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Keys from a small alphabet, so pairs share long prefixes as sorted keys do, of every
    /// length either side of a word's.
    fn key() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(
            prop_oneof![Just(0u8), Just(1), Just(0x7f), Just(0x80), Just(0xff)],
            0..40,
        )
    }

    proptest! {
        /// Against the byte loop and the slices' own order, on keys sharing prefixes of any
        /// length and on a key against its own extensions and truncations.
        #[test]
        fn word_comparisons_agree_with_bytes(a in key(), b in key(), cut in 0usize..40) {
            let bytes = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
            prop_assert_eq!(common_prefix(&a, &b), bytes);
            prop_assert_eq!(less(&a, &b), a < b);
            prop_assert_eq!(equal(&a, &b), a == b);
            let shorter = &a[..cut.min(a.len())];
            prop_assert_eq!(common_prefix(&a, shorter), shorter.len());
            prop_assert_eq!(less(shorter, &a), shorter < a.as_slice());
            prop_assert_eq!(less(&a, shorter), a.as_slice() < shorter);
            prop_assert_eq!(equal(&a, shorter), a.as_slice() == shorter);
        }
    }

    /// The first difference in each byte of a word, and past several words.
    #[test]
    fn a_difference_is_found_at_every_place() {
        for len in 0..33usize {
            let a: Vec<u8> = (0..len).map(|i| u8::try_from(i).unwrap()).collect();
            for at in 0..len {
                let mut b = a.clone();
                b[at] ^= 0x40;
                assert_eq!(common_prefix(&a, &b), at, "len {len} at {at}");
                assert_eq!(less(&a, &b), a < b, "len {len} at {at}");
            }
            assert_eq!(common_prefix(&a, &a), len);
            assert!(equal(&a, &a));
        }
    }
}
