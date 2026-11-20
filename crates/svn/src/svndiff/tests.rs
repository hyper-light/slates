//! The svndiff codec's tests (A-500, §4.17.4 "5. The wire", svndiff): the format specification's own encodings as
//! golden vectors (`notes/svndiff` 49-50, 75-102), every refusal Subversion's decoder makes and the one it misses, and
//! properties over generated windows against a reference applier written here, apart from the module's.

use super::*;
use proptest::prelude::*;

/// The source of `notes/svndiff`'s complete example (86-87).
const EXAMPLE_SOURCE: &[u8] = b"aaaabbbbcccc";

/// The target of `notes/svndiff`'s complete example (86-87).
const EXAMPLE_TARGET: &[u8] = b"aaaaccccdddddddd";

/// `notes/svndiff`'s complete example, byte for byte (89-102): the header, the window's five integers, four
/// instructions and one byte of new data.
const EXAMPLE_V0: &[u8] = &[
  0x53, 0x56, 0x4e, 0x00, // "SVN\0"
  0x00, 0x0c, 0x10, 0x07, 0x01, // source 0 + 12, target 16, 7 instruction bytes, 1 new byte
  0x04, 0x00, // source, length 4, offset 0
  0x04, 0x08, // source, length 4, offset 8
  0x81, // new, length 1
  0x47, 0x08, // target, length 7, offset 8
  0x64, // "d"
];

/// The same window in version 2, derived from `notes/svndiff` 30-43: each section begins with its original length,
/// and a section as long as its original length is stored, which is what Subversion's encoder writes when LZ4 does
/// not shorten it (`compress_lz4.c`): 7 instruction bytes and 1 new byte are never shortened.
const EXAMPLE_V2: &[u8] = &[
  0x53, 0x56, 0x4e, 0x02, // "SVN\2"
  0x00, 0x0c, 0x10, 0x08,
  0x02, // the instruction section is 1 + 7 bytes, the new data's 1 + 1
  0x07, 0x04, 0x00, 0x04, 0x08, 0x81, 0x47, 0x08, // original length 7, then the instructions
  0x01, 0x64, // original length 1, then "d"
];

/// The window `notes/svndiff`'s example decodes to.
fn example_window() -> Window {
  Window {
    source_offset: 0,
    source_len: 12,
    target_len: 16,
    instructions: vec![
      Instruction::Source {
        offset: 0,
        length: 4,
      },
      Instruction::Source {
        offset: 8,
        length: 4,
      },
      Instruction::New { length: 1 },
      Instruction::Target {
        offset: 8,
        length: 7,
      },
    ],
    new_data: b"d".to_vec(),
  }
}

/// A whole delta: the version's header, then each window in order.
fn encode(
  version: Version,
  compression: Compression,
  windows: &[Window],
) -> Result<Vec<u8>, SvndiffError> {
  let mut encoder = Encoder::new(version);
  let mut out = encoder.header().to_vec();
  for window in windows {
    encoder.window(window, compression, &mut out)?;
  }
  Ok(out)
}

/// A version-0 delta of one window made of `fields` (the five header integers) and `body`, which need not agree.
fn raw_delta(fields: [u64; WINDOW_HEADER_FIELDS], body: &[u8]) -> Vec<u8> {
  let mut out = b"SVN\0".to_vec();
  for field in fields {
    write_integer(field, &mut out);
  }
  out.extend_from_slice(body);
  out
}

/// The refusal a version-0 window with this header and body meets.
fn refusal(fields: [u64; WINDOW_HEADER_FIELDS], body: &[u8]) -> SvndiffError {
  decode_all(&raw_delta(fields, body)).unwrap_err()
}

/// The target a window makes, by the plainest reading of `notes/svndiff` 52-70: a reference written apart from the
/// module's applier, byte by byte.
fn reference(source_view: &[u8], window: &Window) -> Vec<u8> {
  let mut out = Vec::new();
  let mut new_at = 0usize;
  for instruction in &window.instructions {
    match *instruction {
      Instruction::Source { offset, length } => {
        for step in 0..length {
          out.push(source_view[usize::try_from(offset + step).unwrap()]);
        }
      }
      Instruction::Target { offset, length } => {
        for step in 0..length {
          let byte = out[usize::try_from(offset + step).unwrap()];
          out.push(byte);
        }
      }
      Instruction::New { length } => {
        for _ in 0..length {
          out.push(window.new_data[new_at]);
          new_at += 1;
        }
      }
    }
  }
  out
}

/// A valid window over a source view of `source_len` bytes starting at `source_offset`, made from generated choices:
/// each choice's action (a source copy, a target copy or new data, falling back to new data where the first two cannot
/// be), its length and its offset, clamped so the window holds together.
fn window_from(
  source_offset: u64,
  source_len: u64,
  choices: &[(u8, u8, u8)],
  pool: &[u8],
) -> Window {
  let mut instructions = Vec::new();
  let mut new_data = Vec::new();
  let mut made = 0u64;
  for &(action, length, offset) in choices {
    let length = u64::from(length % 70) + 1;
    let instruction = match action % 3 {
      0 if source_len > 0 => {
        let offset = u64::from(offset) % source_len;
        Instruction::Source {
          offset,
          length: length.min(source_len - offset),
        }
      }
      1 if made > 0 => Instruction::Target {
        offset: u64::from(offset) % made,
        length,
      },
      _ => {
        for step in 0..length {
          let at = usize::try_from(step).unwrap() + new_data.len();
          new_data.push(pool.get(at % pool.len().max(1)).copied().unwrap_or(0x5a));
        }
        Instruction::New { length }
      }
    };
    made += match instruction {
      Instruction::Source { length, .. }
      | Instruction::Target { length, .. }
      | Instruction::New { length } => length,
    };
    instructions.push(instruction);
  }
  Window {
    source_offset,
    source_len,
    target_len: made,
    instructions,
    new_data,
  }
}

/// A source long enough for three windows' views, made of a generated pool.
fn source_for(pool: &[u8], len: usize) -> Vec<u8> {
  (0..len)
    .map(|at| pool.get(at % pool.len().max(1)).copied().unwrap_or(0x41))
    .collect()
}

/// Decodes `stream` as a caller fed it in the pieces `cuts` makes, holding only what the decoder has not consumed.
#[allow(
  clippy::panic_in_result_fn,
  reason = "a test helper: its assert checks the decoder's NeedMore rule mid-stream, which the decoder's own error type has no variant for; the Result carries the decoder's refusals to the property test"
)]
fn decode_in_pieces(stream: &[u8], cuts: &[usize]) -> Result<Vec<Window>, SvndiffError> {
  let mut decoder = Decoder::new();
  let mut held: Vec<u8> = Vec::new();
  let mut windows = Vec::new();
  let mut bounds: Vec<usize> = cuts.iter().map(|cut| cut % (stream.len() + 1)).collect();
  bounds.push(stream.len());
  bounds.sort_unstable();
  let mut from = 0;
  for to in bounds {
    held.extend_from_slice(&stream[from..to]);
    from = to;
    loop {
      match decoder.decode(&held)? {
        Decoded::Header { consumed, .. } => drop(held.drain(..consumed)),
        Decoded::Window { window, consumed } => {
          windows.push(window);
          drop(held.drain(..consumed));
        }
        Decoded::NeedMore { needed } => {
          assert!(
            needed > held.len(),
            "NeedMore must ask for more than is held: {needed} <= {}",
            held.len()
          );
          break;
        }
      }
    }
  }
  decoder.finish(&held)?;
  Ok(windows)
}

/// A-500 (golden vector, `notes/svndiff` 86-102): decode the specification's complete example, expect its four
/// instructions and one byte of new data, and apply it to "aaaabbbbcccc", expect "aaaaccccdddddddd": the last
/// instruction copies seven bytes from offset 8 of a target only nine bytes long, so the "d" repeats.
#[test]
fn the_specifications_example_decodes_to_its_window_and_makes_its_target() {
  let (version, windows) = decode_all(EXAMPLE_V0).unwrap();
  assert_eq!(version, Version::Zero);
  assert_eq!(windows, vec![example_window()]);
  assert_eq!(apply_all(EXAMPLE_SOURCE, &windows).unwrap(), EXAMPLE_TARGET);
}

/// A-500 (golden vector): encode the specification's example window in version 0, expect its bytes exactly, the
/// integers in their shortest form and lengths under 64 in their instruction's byte, as Subversion's encoder writes.
#[test]
fn the_specifications_example_window_encodes_to_its_own_bytes() {
  assert_eq!(
    encode(Version::Zero, Compression::Stored, &[example_window()]).unwrap(),
    EXAMPLE_V0
  );
}

/// A-500 (golden vector, derived from `notes/svndiff` 30-43 and `compress_lz4.c`): encode the example in version 2
/// stored and with LZ4, expect the same bytes, since LZ4 cannot shorten 7 or 1 bytes; decode them, expect the window.
#[test]
fn the_example_in_version_two_stores_sections_lz4_cannot_shorten() {
  assert_eq!(
    encode(Version::Two, Compression::Stored, &[example_window()]).unwrap(),
    EXAMPLE_V2
  );
  assert_eq!(
    encode(Version::Two, Compression::Lz4, &[example_window()]).unwrap(),
    EXAMPLE_V2
  );
  let (version, windows) = decode_all(EXAMPLE_V2).unwrap();
  assert_eq!(version, Version::Two);
  assert_eq!(windows, vec![example_window()]);
}

/// A-500 (golden vectors, `notes/svndiff` 75-84): read and write the specification's three instruction encodings,
/// expect each instruction and each byte string exactly.
#[test]
fn the_specifications_instruction_encodings_read_and_write_as_written() {
  let cases: [(&[u8], Instruction); 3] = [
    (
      &[0b0000_1011, 0b0000_0000],
      Instruction::Source {
        offset: 0,
        length: 11,
      },
    ),
    (
      &[0b0100_0000, 0b0100_0000, 0b1000_0001, 0b0000_0000],
      Instruction::Target {
        offset: 128,
        length: 64,
      },
    ),
    (&[0b1011_1111], Instruction::New { length: 63 }),
  ];
  for (bytes, instruction) in cases {
    assert_eq!(read_instruction(bytes).unwrap(), (instruction, bytes.len()));
    let mut written = Vec::new();
    write_instruction(instruction, &mut written);
    assert_eq!(written, bytes);
  }
}

/// A-500 (golden vector, `notes/svndiff` 49-50): write 130, expect 10000001 00000010, and read it back; write the
/// edges, expect the shortest form read back whole, u64::MAX in ten bytes.
#[test]
fn integers_are_written_shortest_and_read_back_whole() {
  let mut written = Vec::new();
  write_integer(130, &mut written);
  assert_eq!(written, [0b1000_0001, 0b0000_0010]);
  for (value, length) in [
    (0, 1),
    (127, 1),
    (128, 2),
    (16_383, 2),
    (16_384, 3),
    (u64::MAX, INTEGER_LIMIT),
  ] {
    let mut bytes = Vec::new();
    write_integer(value, &mut bytes);
    assert_eq!(bytes.len(), length, "{value}");
    assert_eq!(read_integer(&bytes), Ok(Some((value, length))));
  }
}

/// A-500 (hostile input): read integers that run past ten bytes or past 64 bits, expect each refused, where
/// Subversion's `svn__decode_uint` truncates the second; read a cut integer, expect a call for more bytes.
#[test]
fn an_integer_past_ten_bytes_or_sixty_four_bits_is_refused() {
  assert_eq!(
    read_integer(&[0x80; INTEGER_LIMIT + 1]),
    Err(IntegerFault::TooLong)
  );
  let two_to_the_sixty_four = [0x82, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
  assert_eq!(
    read_integer(&two_to_the_sixty_four),
    Err(IntegerFault::Overflow)
  );
  assert_eq!(read_integer(&[0x81, 0x80]), Ok(None));
  let mut delta = b"SVN\0".to_vec();
  delta.extend_from_slice(&two_to_the_sixty_four);
  assert_eq!(decode_all(&delta), Err(SvndiffError::CorruptWindowHeader));
}

/// A-500 (hostile input, `svndiff.c` `write_handler`): decode a header that is foreign at its first byte, at its
/// third, of version 1 (never offered) or of version 3, expect each refused with Subversion's E185000; decode an
/// empty or cut header, expect a call for four bytes and, at the end, E185004.
#[test]
fn a_foreign_cut_or_unoffered_header_is_refused() {
  for (bytes, refused) in [
    (&b"X"[..], SvndiffError::InvalidHeader),
    (&b"SVX"[..], SvndiffError::InvalidHeader),
    (&b"SVN\x01"[..], SvndiffError::VersionNotOffered),
    (&b"SVN\x03"[..], SvndiffError::InvalidHeader),
  ] {
    assert_eq!(Decoder::new().decode(bytes), Err(refused));
    assert_eq!(refused.subversion_code(), 185_000);
  }
  assert_eq!(
    Decoder::new().decode(b"SV"),
    Ok(Decoded::NeedMore { needed: HEADER_LEN })
  );
  assert_eq!(decode_all(b"SV"), Err(SvndiffError::UnexpectedEnd));
  assert_eq!(decode_all(b""), Err(SvndiffError::UnexpectedEnd));
  assert_eq!(SvndiffError::UnexpectedEnd.subversion_code(), 185_004);
  assert_eq!(decode_all(b"SVN\0"), Ok((Version::Zero, Vec::new())));
}

/// A-500 (hostile input, `svndiff.c` 714-728): decode window headers one past each limit with no body behind them,
/// expect E185001 at once, before any body is awaited; at each limit, expect only a call for the body's bytes.
#[test]
fn a_window_past_the_formats_limits_is_refused_before_its_body() {
  let over = [
    [0, WINDOW_LIMIT + 1, 0, 0, 0],
    [0, 0, WINDOW_LIMIT + 1, 0, 0],
    [0, 0, 0, INSTRUCTION_SECTION_LIMIT + 1, 0],
    [0, 0, 0, 0, NEW_DATA_SECTION_LIMIT + 1],
    [u64::from(u32::MAX), 0, 0, 0, u64::from(u32::MAX)],
  ];
  for fields in over {
    assert_eq!(
      refusal(fields, &[]),
      SvndiffError::WindowTooLarge,
      "{fields:?}"
    );
  }
  let at_the_limits = raw_delta(
    [
      0,
      WINDOW_LIMIT,
      WINDOW_LIMIT,
      INSTRUCTION_SECTION_LIMIT,
      NEW_DATA_SECTION_LIMIT,
    ],
    &[],
  );
  let body = &at_the_limits[HEADER_LEN..];
  let decoded = {
    let mut decoder = Decoder::new();
    decoder.decode(&at_the_limits).unwrap();
    decoder.decode(body).unwrap()
  };
  let Decoded::NeedMore { needed } = decoded else {
    panic!("{decoded:?}")
  };
  let limits = usize::try_from(INSTRUCTION_SECTION_LIMIT + NEW_DATA_SECTION_LIMIT).unwrap();
  assert_eq!(needed, body.len() + limits);
  assert_eq!(SvndiffError::WindowTooLarge.subversion_code(), 185_001);
}

/// A-500 (hostile input, `svndiff.c` 722-728): decode a source view starting past Subversion's signed 64-bit offsets,
/// expect E185001; at the last signed offset, expect the window read.
#[test]
fn a_source_offset_past_signed_offsets_is_refused() {
  let signed_limit = u64::try_from(i64::MAX).unwrap();
  assert_eq!(
    refusal([signed_limit + 1, 0, 0, 0, 0], &[]),
    SvndiffError::CorruptWindowHeader
  );
  let (_, windows) = decode_all(&raw_delta([signed_limit, 0, 0, 0, 0], &[])).unwrap();
  assert_eq!(windows.len(), 1);
}

/// A-500 (hostile input, `svndiff.c` 486-543): decode one window per instruction fault, expect each refused with E185003
/// naming the instruction's place and fault: the selector 11, a zero length, a copy past the target, past the source,
/// from the target's unwritten end, past the new data, an instruction cut by its section's end; then a target left
/// unfilled and new data left unused.
#[test]
fn each_malformed_instruction_is_refused_with_its_place_and_fault() {
  use InstructionFault::*;
  let faults: [([u64; WINDOW_HEADER_FIELDS], &[u8], usize, InstructionFault); 7] = [
    ([0, 0, 4, 1, 0], &[0xc4], 0, Undecodable),
    ([0, 4, 4, 3, 0], &[0x00, 0x00, 0x00], 0, ZeroLength),
    ([0, 0, 4, 1, 5], &[0x85, 1, 2, 3, 4, 5], 0, PastTarget),
    ([0, 4, 4, 2, 0], &[0x04, 0x01], 0, PastSource),
    (
      [0, 0, 4, 3, 2],
      &[0x82, 0x42, 0x02, 1, 2],
      1,
      TargetNotBehind,
    ),
    ([0, 0, 4, 1, 2], &[0x84, 1, 2], 0, PastNewData),
    ([0, 0, 4, 2, 0], &[0x40, 0x84], 0, Undecodable),
  ];
  for (fields, body, index, fault) in faults {
    assert_eq!(
      refusal(fields, body),
      SvndiffError::InvalidInstruction { index, fault },
      "{fields:?} {body:?}"
    );
  }
  assert_eq!(
    refusal([0, 0, 4, 1, 3], &[0x83, 1, 2, 3]),
    SvndiffError::TargetNotFilled
  );
  assert_eq!(
    refusal([0, 0, 2, 1, 3], &[0x82, 1, 2, 3]),
    SvndiffError::NewDataUnused
  );
  assert_eq!(SvndiffError::NewDataUnused.subversion_code(), 185_003);
}

/// A-500 (hostile input, `svndiff.c` 730-737): decode windows whose source view starts before the previous one's, or
/// ends before it, expect E185002; decode a view that follows an empty one from further back, expect it read, since
/// Subversion judges only a window with a source view, against the previous window whatever its view.
#[test]
fn a_source_view_sliding_backward_is_refused_as_subversion_judges_it() {
  let view = |offset, len| Window {
    source_offset: offset,
    source_len: len,
    target_len: 0,
    instructions: Vec::new(),
    new_data: Vec::new(),
  };
  let mut stream = b"SVN\0".to_vec();
  for (offset, len) in [(100, 50), (90, 50)] {
    for field in [offset, len, 0, 0, 0] {
      write_integer(field, &mut stream);
    }
  }
  assert_eq!(decode_all(&stream), Err(SvndiffError::BackwardSourceView));
  assert_eq!(SvndiffError::BackwardSourceView.subversion_code(), 185_002);
  let mut encoder = Encoder::new(Version::Zero);
  let mut out = Vec::new();
  encoder
    .window(&view(100, 50), Compression::Stored, &mut out)
    .unwrap();
  assert_eq!(
    encoder.window(&view(100, 40), Compression::Stored, &mut out),
    Err(SvndiffError::BackwardSourceView)
  );
  let quirk = encode(
    Version::Zero,
    Compression::Stored,
    &[view(100, 50), view(0, 0), view(0, 10)],
  )
  .unwrap();
  assert_eq!(decode_all(&quirk).unwrap().1.len(), 3);
}

/// A-500 (hostile input, `compress_lz4.c` `svn__decompress_lz4`): decode version-2 sections with no original length,
/// an original length past its limit, a block LZ4 cannot read and a block shorter than the length it claims, expect
/// E185005, E185005, E200046 and E185005.
#[test]
fn version_two_sections_with_a_bad_length_or_block_are_refused() {
  let delta = |sections: &[u8], instructions_len: u64| {
    let mut out = b"SVN\x02".to_vec();
    let new_len = u64::try_from(sections.len()).unwrap() - instructions_len;
    for field in [0, 0, 0, instructions_len, new_len] {
      write_integer(field, &mut out);
    }
    out.extend_from_slice(sections);
    out
  };
  assert_eq!(
    decode_all(&delta(&[0x00], 0)),
    Err(SvndiffError::InvalidCompressedData)
  );
  let mut too_long = Vec::new();
  write_integer(WINDOW_LIMIT + 1, &mut too_long);
  assert_eq!(
    decode_all(&delta(&[&[0x00][..], &too_long].concat(), 1)),
    Err(SvndiffError::InvalidCompressedData)
  );
  assert_eq!(
    decode_all(&delta(&[0x00, 0x64, 0xff, 0xff], 1)),
    Err(SvndiffError::Lz4)
  );
  let block = lz4_flex::block::compress(&[7u8; 40]);
  let mut claims_more = Vec::new();
  write_integer(41, &mut claims_more);
  claims_more.extend_from_slice(&block);
  let outcome = decode_all(&delta(&[&[0x00][..], &claims_more].concat(), 1));
  assert!(
    matches!(
      outcome,
      Err(SvndiffError::InvalidCompressedData | SvndiffError::Lz4)
    ),
    "{outcome:?}"
  );
  assert_eq!(
    SvndiffError::InvalidCompressedData.subversion_code(),
    185_005
  );
  assert_eq!(SvndiffError::Lz4.subversion_code(), 200_046);
}

/// A-500 (version 2 with LZ4): encode a window of 4,096 repeated bytes of new data with LZ4, expect a delta shorter
/// than the stored one, decoding to the same window and target.
#[test]
fn a_compressible_section_is_an_lz4_block_and_reads_back() {
  let window = Window {
    source_offset: 0,
    source_len: 0,
    target_len: 4_096,
    instructions: vec![Instruction::New { length: 4_096 }],
    new_data: vec![b'a'; 4_096],
  };
  let stored = encode(
    Version::Two,
    Compression::Stored,
    std::slice::from_ref(&window),
  )
  .unwrap();
  let compressed = encode(
    Version::Two,
    Compression::Lz4,
    std::slice::from_ref(&window),
  )
  .unwrap();
  assert!(
    compressed.len() < stored.len() / 10,
    "{} of {}",
    compressed.len(),
    stored.len()
  );
  let (_, windows) = decode_all(&compressed).unwrap();
  assert_eq!(windows, vec![window]);
  assert_eq!(apply_all(&[], &windows).unwrap(), vec![b'a'; 4_096]);
}

/// A-500 (`text_delta.c` 785): apply a window to a source shorter than its view, expect E200003 and the target
/// untouched; encode a window past the target limit, expect it refused and nothing written.
#[test]
fn a_short_source_and_an_oversized_window_are_refused_untouched() {
  let mut target = b"kept".to_vec();
  assert_eq!(
    apply(&example_window(), b"aaaabbbb", &mut target),
    Err(SvndiffError::SourceShort)
  );
  assert_eq!(target, b"kept");
  assert_eq!(SvndiffError::SourceShort.subversion_code(), 200_003);
  let length = WINDOW_LIMIT + 1;
  let oversized = Window {
    source_offset: 0,
    source_len: 0,
    target_len: length,
    instructions: vec![Instruction::New { length }],
    new_data: vec![0; usize::try_from(length).unwrap()],
  };
  let mut out = Vec::new();
  assert_eq!(
    Encoder::new(Version::Zero).window(&oversized, Compression::Stored, &mut out),
    Err(SvndiffError::WindowTooLarge)
  );
  assert!(out.is_empty());
}

/// A-500 (hostile input): cut a two-window delta at every byte, expect every cut refused E185004 but the two that
/// fall between windows, which decode to the windows before them.
#[test]
fn a_delta_cut_anywhere_but_between_windows_is_refused() {
  let second = Window {
    source_offset: 12,
    ..example_window()
  };
  let full = encode(
    Version::Zero,
    Compression::Stored,
    &[example_window(), second],
  )
  .unwrap();
  let window_len = EXAMPLE_V0.len() - HEADER_LEN;
  for cut in 0..full.len() {
    let outcome = decode_all(&full[..cut]);
    match cut {
      _ if cut == HEADER_LEN => assert_eq!(outcome.unwrap().1.len(), 0),
      _ if cut == HEADER_LEN + window_len => assert_eq!(outcome.unwrap().1.len(), 1),
      _ => assert_eq!(outcome, Err(SvndiffError::UnexpectedEnd), "cut at {cut}"),
    }
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-500 (oracle): for generated windows, encode in version 0, version 2 stored and version 2 with LZ4, expect each
  /// to decode to the same windows and to encode again to the same bytes; apply them, expect the reference's target.
  #[test]
  fn any_window_round_trips_in_every_encoding_and_applies_as_the_reference_does(
    pool in proptest::collection::vec(any::<u8>(), 1..64),
    views in proptest::collection::vec((0u64..=64, proptest::collection::vec(any::<(u8, u8, u8)>(), 0..48)), 1..4),
  ) {
    let windows: Vec<Window> = views
      .iter()
      .enumerate()
      .map(|(index, (len, choices))| window_from(u64::try_from(index).unwrap() * 64, *len, choices, &pool))
      .collect();
    let source = source_for(&pool, 4 * 64);
    let mut expected = Vec::new();
    for window in &windows {
      let start = usize::try_from(window.source_offset).unwrap();
      let view = &source[start..start + usize::try_from(window.source_len).unwrap()];
      expected.extend(reference(view, window));
    }
    for (version, compression) in [(Version::Zero, Compression::Stored), (Version::Two, Compression::Stored), (Version::Two, Compression::Lz4)] {
      let bytes = encode(version, compression, &windows).unwrap();
      let (read_version, read) = decode_all(&bytes).unwrap();
      prop_assert_eq!(read_version, version);
      prop_assert_eq!(&read, &windows);
      prop_assert_eq!(encode(version, compression, &read).unwrap(), bytes);
      prop_assert_eq!(apply_all(&source, &read).unwrap(), expected.clone());
    }
  }

  /// A-500 (hostile input): decode any bytes after either header, expect a typed answer and never a panic; whatever
  /// decodes encodes again and decodes to the same windows.
  #[test]
  fn any_bytes_decode_to_a_typed_answer(two in any::<bool>(), body in proptest::collection::vec(any::<u8>(), 0..512)) {
    let mut delta = if two { b"SVN\x02".to_vec() } else { b"SVN\0".to_vec() };
    delta.extend_from_slice(&body);
    if let Ok((version, windows)) = decode_all(&delta) {
      let again = encode(version, Compression::Stored, &windows).unwrap();
      prop_assert_eq!(decode_all(&again).unwrap(), (version, windows));
    }
  }

  /// A-500 (split invariance): feed a valid delta to the decoder in any pieces, holding only what it has not consumed,
  /// expect the windows a whole read gives, and every call for more asking for more than is held.
  #[test]
  fn a_delta_fed_in_any_pieces_decodes_to_the_same_windows(
    pool in proptest::collection::vec(any::<u8>(), 1..32),
    choices in proptest::collection::vec(proptest::collection::vec(any::<(u8, u8, u8)>(), 0..24), 1..4),
    cuts in proptest::collection::vec(any::<usize>(), 0..12),
    two in any::<bool>(),
  ) {
    let windows: Vec<Window> = choices
      .iter()
      .enumerate()
      .map(|(index, choices)| window_from(u64::try_from(index).unwrap() * 32, 32, choices, &pool))
      .collect();
    let version = if two { Version::Two } else { Version::Zero };
    let bytes = encode(version, Compression::Lz4, &windows).unwrap();
    prop_assert_eq!(decode_in_pieces(&bytes, &cuts).unwrap(), windows);
  }
}
