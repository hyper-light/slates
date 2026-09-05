//! The `Wire` trait and the canonical primitive encodings: fixed-width little-endian integers,
//! `bool` and `Option` tags of exactly 0 or 1, `u32`-prefixed strings and vectors, raw byte
//! arrays, canonical floats (no negative zero, one NaN). Every decoder checks the input before
//! allocating and refuses a non-canonical byte, so a hostile body costs at most its own length.

use crate::error::WireError;

/// A type with one canonical encoding, a reflection and a schema hash.
pub trait Wire: Sized {
  /// The reflection text (`struct Name{field:type,...}`).
  const SCHEMA: &'static str;
  /// The structural hash of the reflection and every field type.
  const SCHEMA_HASH: u64;
  /// Appends the canonical encoding.
  fn encode(&self, out: &mut Vec<u8>);
  /// Reads one value from the front of `input`, advancing it.
  fn decode(input: &mut &[u8]) -> Result<Self, WireError>;

  /// The whole encoding as bytes.
  fn to_bytes(&self) -> Vec<u8> {
    let mut out = Vec::new();
    self.encode(&mut out);
    out
  }

  /// Decodes a whole body, refusing trailing bytes.
  fn from_bytes(bytes: &[u8]) -> Result<Self, WireError> {
    let mut input = bytes;
    let value = Self::decode(&mut input)?;
    if !input.is_empty() {
      return Err(WireError::TrailingBytes { count: input.len() });
    }
    Ok(value)
  }
}

/// Takes `n` bytes from the front of the input.
pub fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8], WireError> {
  if input.len() < n {
    return Err(WireError::Truncated {
      needed: n - input.len(),
    });
  }
  let (head, tail) = input.split_at(n);
  *input = tail;
  Ok(head)
}

/// Reads a `u32` length prefix and checks that many bytes (or elements of at least one byte
/// each) remain, so nothing is allocated for a length the input cannot hold.
pub fn take_len(input: &mut &[u8]) -> Result<usize, WireError> {
  let len = usize::try_from(u32::decode(input)?).unwrap_or(usize::MAX);
  if input.len() < len {
    return Err(WireError::Truncated {
      needed: len - input.len(),
    });
  }
  Ok(len)
}

macro_rules! integer {
  ($($t:ty),*) => {$(
    impl Wire for $t {
      const SCHEMA: &'static str = stringify!($t);
      const SCHEMA_HASH: u64 = crate::schema::fnv64(stringify!($t));
      fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
      }
      fn decode(input: &mut &[u8]) -> Result<Self, WireError> {
        let bytes = take(input, std::mem::size_of::<$t>())?;
        let mut word = [0u8; std::mem::size_of::<$t>()];
        word.copy_from_slice(bytes);
        Ok(<$t>::from_le_bytes(word))
      }
    }
  )*};
}

integer!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);

impl Wire for bool {
  const SCHEMA: &'static str = "bool";
  const SCHEMA_HASH: u64 = crate::schema::fnv64("bool");
  fn encode(&self, out: &mut Vec<u8>) {
    out.push(u8::from(*self));
  }
  fn decode(input: &mut &[u8]) -> Result<Self, WireError> {
    match u8::decode(input)? {
      0 => Ok(false),
      1 => Ok(true),
      got => Err(WireError::BadTag { got }),
    }
  }
}

impl Wire for f64 {
  const SCHEMA: &'static str = "f64";
  const SCHEMA_HASH: u64 = crate::schema::fnv64("f64");
  fn encode(&self, out: &mut Vec<u8>) {
    let canonical = if self.is_nan() {
      f64::NAN
    } else if *self == 0.0 {
      0.0
    } else {
      *self
    };
    out.extend_from_slice(&canonical.to_bits().to_le_bytes());
  }
  fn decode(input: &mut &[u8]) -> Result<Self, WireError> {
    let bits = u64::decode(input)?;
    let value = f64::from_bits(bits);
    let canonical = if value.is_nan() {
      f64::NAN.to_bits()
    } else if value == 0.0 {
      0
    } else {
      bits
    };
    if bits != canonical {
      return Err(WireError::NonCanonicalFloat);
    }
    Ok(value)
  }
}

impl Wire for String {
  const SCHEMA: &'static str = "String";
  const SCHEMA_HASH: u64 = crate::schema::fnv64("String");
  fn encode(&self, out: &mut Vec<u8>) {
    len_prefix(self.len(), out);
    out.extend_from_slice(self.as_bytes());
  }
  fn decode(input: &mut &[u8]) -> Result<Self, WireError> {
    let len = take_len(input)?;
    let bytes = take(input, len)?;
    std::str::from_utf8(bytes)
      .map(str::to_owned)
      .map_err(|_| WireError::BadUtf8)
  }
}

impl<T: Wire> Wire for Vec<T> {
  const SCHEMA: &'static str = "Vec";
  const SCHEMA_HASH: u64 = crate::schema::mix(crate::schema::fnv64("Vec"), &[T::SCHEMA_HASH]);
  fn encode(&self, out: &mut Vec<u8>) {
    len_prefix(self.len(), out);
    for item in self {
      item.encode(out);
    }
  }
  fn decode(input: &mut &[u8]) -> Result<Self, WireError> {
    let len = take_len(input)?;
    let mut items = Vec::with_capacity(len.min(input.len()));
    for _ in 0..len {
      items.push(T::decode(input)?);
    }
    Ok(items)
  }
}

impl<T: Wire> Wire for Option<T> {
  const SCHEMA: &'static str = "Option";
  const SCHEMA_HASH: u64 = crate::schema::mix(crate::schema::fnv64("Option"), &[T::SCHEMA_HASH]);
  fn encode(&self, out: &mut Vec<u8>) {
    match self {
      None => out.push(0),
      Some(v) => {
        out.push(1);
        v.encode(out);
      }
    }
  }
  fn decode(input: &mut &[u8]) -> Result<Self, WireError> {
    match u8::decode(input)? {
      0 => Ok(None),
      1 => Ok(Some(T::decode(input)?)),
      got => Err(WireError::BadTag { got }),
    }
  }
}

impl<const N: usize> Wire for [u8; N] {
  const SCHEMA: &'static str = "[u8;N]";
  const SCHEMA_HASH: u64 = crate::schema::mix(crate::schema::fnv64("[u8;N]"), &[N as u64]);
  fn encode(&self, out: &mut Vec<u8>) {
    out.extend_from_slice(self);
  }
  fn decode(input: &mut &[u8]) -> Result<Self, WireError> {
    let bytes = take(input, N)?;
    let mut out = [0u8; N];
    out.copy_from_slice(bytes);
    Ok(out)
  }
}

/// Writes a `u32` length prefix; a length beyond `u32` is a bug in the caller and is clamped so
/// the receiver refuses the frame rather than the sender panicking.
fn len_prefix(len: usize, out: &mut Vec<u8>) {
  u32::try_from(len).unwrap_or(u32::MAX).encode(out);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn primitives_round_trip_in_little_endian() {
    assert_eq!(0x0102_0304u32.to_bytes(), vec![4, 3, 2, 1]);
    assert_eq!(u64::from_bytes(&7u64.to_bytes()).unwrap(), 7);
    assert_eq!(i16::from_bytes(&(-2i16).to_bytes()).unwrap(), -2);
    assert_eq!(true.to_bytes(), vec![1]);
    assert_eq!(String::from("hi").to_bytes(), vec![2, 0, 0, 0, b'h', b'i']);
    assert_eq!(Some(9u8).to_bytes(), vec![1, 9]);
    assert_eq!(None::<u8>.to_bytes(), vec![0]);
    assert_eq!(vec![1u16, 2].to_bytes(), vec![2, 0, 0, 0, 1, 0, 2, 0]);
    assert_eq!(<[u8; 3]>::from_bytes(&[7, 8, 9]).unwrap(), [7, 8, 9]);
  }

  #[test]
  fn bad_tags_and_short_or_long_inputs_are_refused() {
    assert!(matches!(
      bool::from_bytes(&[2]),
      Err(WireError::BadTag { got: 2 })
    ));
    assert!(matches!(
      Option::<u8>::from_bytes(&[7, 0]),
      Err(WireError::BadTag { got: 7 })
    ));
    assert!(matches!(
      u32::from_bytes(&[1, 2]),
      Err(WireError::Truncated { needed: 2 })
    ));
    assert!(matches!(
      u8::from_bytes(&[1, 2]),
      Err(WireError::TrailingBytes { count: 1 })
    ));
  }

  #[test]
  fn bad_utf8_and_non_canonical_floats_are_refused_and_the_encoder_canonicalizes() {
    assert!(matches!(
      String::from_bytes(&[1, 0, 0, 0, 0xFF]),
      Err(WireError::BadUtf8)
    ));
    assert!(matches!(
      f64::from_bytes(&(-0.0f64).to_bits().to_le_bytes()),
      Err(WireError::NonCanonicalFloat)
    ));
    assert_eq!(
      (-0.0f64).to_bytes(),
      0.0f64.to_bytes(),
      "the encoder canonicalizes"
    );
  }

  #[test]
  fn a_hostile_length_allocates_nothing() {
    let mut bytes = u32::MAX.to_bytes();
    bytes.push(0);
    let err = Vec::<u8>::from_bytes(&bytes).unwrap_err();
    assert!(matches!(err, WireError::Truncated { .. }), "{err}");
    let err = String::from_bytes(&bytes).unwrap_err();
    assert!(matches!(err, WireError::Truncated { .. }), "{err}");
  }
}
