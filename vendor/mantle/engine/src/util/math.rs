//! Bit arithmetic: RocksDB's `util/math.h`, with the 128-bit specializations of
//! `util/math128.h`.
//!
//! Where RocksDB is undefined — `FloorLog2` of zero or a negative value, `CountTrailingZeroBits`
//! of zero, `BottomNBits` at the full width or more — these are total: the first two return
//! `None`, and `bottom_n_bits` returns the whole value. `BitwiseAnd`, which picks the narrower
//! C++ operand type, and `ConstexprFloorLog2` have no Rust counterpart to convert: Rust does not
//! widen operands implicitly, and [`BitMath::floor_log2`] covers both uses.

/// The operations of `util/math.h` on one integer type.
pub trait BitMath: Sized + Copy {
    /// `BottomNBits` [R util/math.h:25-45]: the low `nbits` bits.
    fn bottom_n_bits(self, nbits: u32) -> Self;
    /// `FloorLog2` [R util/math.h:47-89]: the position of the highest 1 bit.
    fn floor_log2(self) -> Option<u32>;
    /// `CountTrailingZeroBits` [R util/math.h:103-138].
    fn count_trailing_zero_bits(self) -> Option<u32>;
    /// `BitsSetToOne` [R util/math.h:171-222], the population count of the type's own bits.
    fn bits_set_to_one(self) -> u32;
    /// `BitParity` [R util/math.h:224-243].
    fn bit_parity(self) -> u32;
    /// `EndianSwapValue` [R util/math.h:245-272].
    fn endian_swap_value(self) -> Self;
    /// `ReverseBits` [R util/math.h:274-288].
    fn reverse_bits_value(self) -> Self;
    /// `DownwardInvolution` [R util/math.h:290-331]: its own inverse, keeps the highest 1 bit
    /// where it was, distributes over xor, and makes each output bit depend on the input bits at
    /// and above it.
    fn downward_involution(self) -> Self;
}

/// `DownwardInvolution` on the low `bits` bits of `r` (`bits <= 64`); a wider type's bits above
/// 64 do not exist here, and a narrower one's are zero, so no step mixes in bits the type does
/// not have [R util/math.h:318-330].
const fn downward_involution64(mut r: u64, bits: u32) -> u64 {
    if bits > 32 {
        r ^= r >> 32;
    }
    if bits > 16 {
        r ^= (r & 0xffff_0000_ffff_0000) >> 16;
    }
    if bits > 8 {
        r ^= (r & 0xff00_ff00_ff00_ff00) >> 8;
    }
    r ^= (r & 0xf0f0_f0f0_f0f0_f0f0) >> 4;
    r ^= (r & 0xcccc_cccc_cccc_cccc) >> 2;
    r ^= (r & 0xaaaa_aaaa_aaaa_aaaa) >> 1;
    r
}

macro_rules! bit_math {
    ($t:ty, $u:ty, $to_u:expr, $from_u:expr, $to64:expr, $from64:expr) => {
        impl BitMath for $t {
            fn bottom_n_bits(self, nbits: u32) -> Self {
                let u: $u = $to_u(self);
                match (1 as $u).checked_shl(nbits) {
                    Some(bit) => $from_u(u & bit.wrapping_sub(1)),
                    None => self,
                }
            }
            fn floor_log2(self) -> Option<u32> {
                self.checked_ilog2()
            }
            fn count_trailing_zero_bits(self) -> Option<u32> {
                (self != 0).then(|| self.trailing_zeros())
            }
            fn bits_set_to_one(self) -> u32 {
                self.count_ones()
            }
            fn bit_parity(self) -> u32 {
                self.count_ones() & 1
            }
            fn endian_swap_value(self) -> Self {
                self.swap_bytes()
            }
            fn reverse_bits_value(self) -> Self {
                self.reverse_bits()
            }
            fn downward_involution(self) -> Self {
                let u: $u = $to_u(self);
                let r = downward_involution64($to64(u), <$t>::BITS);
                // The steps only move bits down, so `r` fits the type again.
                let back: Option<$u> = $from64(r);
                $from_u(back.unwrap_or(u))
            }
        }
    };
}

// Each type's unsigned form, and its widening to and narrowing from the 64-bit word the
// involution runs in (every type here is at most 64 bits wide).
bit_math!(u8, u8, |v| v, |v| v, u64::from, |r| u8::try_from(r).ok());
bit_math!(u16, u16, |v| v, |v| v, u64::from, |r| u16::try_from(r).ok());
bit_math!(u32, u32, |v| v, |v| v, u64::from, |r| u32::try_from(r).ok());
bit_math!(u64, u64, |v| v, |v| v, |v| v, Some);
bit_math!(usize, usize, |v| v, |v| v, |v: usize| v as u64, |r| {
    usize::try_from(r).ok()
});
bit_math!(i8, u8, i8::cast_unsigned, u8::cast_signed, u64::from, |r| {
    u8::try_from(r).ok()
});
bit_math!(
    i16,
    u16,
    i16::cast_unsigned,
    u16::cast_signed,
    u64::from,
    |r| u16::try_from(r).ok()
);
bit_math!(
    i32,
    u32,
    i32::cast_unsigned,
    u32::cast_signed,
    u64::from,
    |r| u32::try_from(r).ok()
);
bit_math!(i64, u64, i64::cast_unsigned, u64::cast_signed, |v| v, Some);
bit_math!(
    isize,
    usize,
    isize::cast_unsigned,
    usize::cast_signed,
    |v: usize| v as u64,
    |r| usize::try_from(r).ok()
);

/// The 128-bit forms [R util/math128.h:193-247].
impl BitMath for u128 {
    fn bottom_n_bits(self, nbits: u32) -> Self {
        match 1u128.checked_shl(nbits) {
            Some(bit) => self & bit.wrapping_sub(1),
            None => self,
        }
    }
    fn floor_log2(self) -> Option<u32> {
        self.checked_ilog2()
    }
    fn count_trailing_zero_bits(self) -> Option<u32> {
        (self != 0).then(|| self.trailing_zeros())
    }
    fn bits_set_to_one(self) -> u32 {
        self.count_ones()
    }
    fn bit_parity(self) -> u32 {
        self.count_ones() & 1
    }
    fn endian_swap_value(self) -> Self {
        self.swap_bytes()
    }
    fn reverse_bits_value(self) -> Self {
        self.reverse_bits()
    }
    /// The upper word's involution above that of the xor of both words
    /// [R util/math128.h:243-247].
    fn downward_involution(self) -> Self {
        use crate::util::math128::{lower64of128, upper64of128};
        let hi = upper64of128(self);
        let lo = lower64of128(self);
        (u128::from(downward_involution64(hi, 64)) << 64)
            | u128::from(downward_involution64(hi ^ lo, 64))
    }
}
