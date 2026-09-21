//! Instruction-count benches for the wire under callgrind (iai-callgrind): header encode and
//! decode, a sample body's encode and decode, a frame's encode and decode, CRC32C over a page
//! (D-20; BENCHMARKS.md "Ratchets").
//! Teardown verifies decoded values and encoded round trips. Refused operations
//! fail the run; a corrupt-frame negative control previously exited successfully (2026-09-20).

// The benchmark macros generate undocumented modules and constants; the doc rule is for our
// own items, which are documented above each function.
#![allow(missing_docs)]

use std::hint::black_box;

use iai_callgrind::{
  Callgrind, EntryPoint, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
};
use slates_wire::crc32c::crc32c;
use slates_wire::frame::{Frame, KindEntry};
use slates_wire::header::{Class, Flags, Header};
use slates_wire::{FrameCaps, Framer, RequestId, Wire, WireError};

// The default function-return boundary included teardown instructions on Linux ARM64
// (2026-09-20: the CRC oracle added 119,143 instructions). Explicit requests bracket
// the advertised operation, including its owned input drops, before verification begins.
// Keep a real call boundary: Valgrind 3.24 on ARM64 lost the entire header
// encode from totals when both collection requests were inlined (20 summary, 0 total).
#[cfg(target_os = "linux")]
#[inline(never)]
fn toggle_collection() {
  iai_callgrind::client_requests::callgrind::toggle_collect();
}

// Valgrind is only run in the Linux lane; other hosts still run the same result checks.
#[cfg(not(target_os = "linux"))]
fn toggle_collection() {}

// The closure owns each input, so its destruction finishes before collection stops.
// Both boundaries stay inside this call; teardown size cannot alter the measured return path.
fn count_instructions<Value>(operation: impl FnOnce() -> Value) -> Value {
  toggle_collection();
  let result = operation();
  toggle_collection();
  result
}

// The runner owns main and requires infallible setup/teardown functions. Report the typed
// refusal and fail this benchmark process at that boundary; never substitute a fixture.
fn require_success<Value>(result: Result<Value, impl std::fmt::Debug>, operation: &str) -> Value {
  match result {
    Ok(value) => value,
    Err(error) => {
      eprintln!("{operation}: {error:?}");
      std::process::exit(1);
    }
  }
}

#[derive(Debug, PartialEq, Eq, Wire)]
struct Sample {
  name: String,
  id: [u8; 16],
  size: u64,
  flags: Vec<u32>,
  note: Option<String>,
}

fn sample() -> Sample {
  Sample {
    name: "volume-1234".to_owned(),
    id: [7; 16],
    size: 1 << 30,
    flags: vec![1, 2, 3, 4],
    note: Some("witnessed".to_owned()),
  }
}

fn header() -> Header {
  Header {
    minor: 0,
    flags: Flags::END_OF_STREAM,
    class: Class::Metadata,
    kind: 3,
    length: 128,
    checksum: 0x1234_5678,
    request: RequestId {
      client: 1,
      sequence: 2,
    },
  }
}

fn framer() -> Framer {
  Framer::new(
    FrameCaps::explicit(1 << 16, 1 << 16, 1 << 20, 1 << 10),
    vec![KindEntry {
      class: Class::Metadata,
      kind: 3,
      schema_hash: Sample::SCHEMA_HASH,
    }],
  )
}

fn check_header_encode(bytes: [u8; 32]) {
  assert_eq!(Header::decode(&bytes), Ok(header()));
}

#[library_benchmark(teardown = check_header_encode)]
#[bench::encode(header())]
fn header_encode(header: Header) -> [u8; 32] {
  count_instructions(move || black_box(header.encode()))
}

fn check_header_decode(result: Result<Header, WireError>) {
  assert_eq!(result, Ok(header()));
}

#[library_benchmark(teardown = check_header_decode)]
#[bench::decode(header().encode())]
fn header_decode(bytes: [u8; 32]) -> Result<Header, WireError> {
  count_instructions(move || black_box(Header::decode(&bytes)))
}

fn check_body_encode(bytes: Vec<u8>) {
  assert_eq!(Sample::from_bytes(&bytes), Ok(sample()));
}

#[library_benchmark(teardown = check_body_encode)]
#[bench::body(sample())]
fn body_encode(sample: Sample) -> Vec<u8> {
  count_instructions(move || black_box(sample.to_bytes()))
}

fn check_body_decode(result: Result<Sample, WireError>) {
  assert_eq!(result, Ok(sample()));
}

#[library_benchmark(teardown = check_body_decode)]
#[bench::bytes(sample().to_bytes())]
fn body_decode(bytes: Vec<u8>) -> Result<Sample, WireError> {
  count_instructions(move || black_box(Sample::from_bytes(&bytes)))
}

fn check_frame((frame, remaining): (Frame, usize)) {
  assert_eq!(
    remaining, 0,
    "decode consumes exactly the one encoded frame"
  );
  assert_eq!(frame.header.class, Class::Metadata);
  assert_eq!(frame.header.kind, 3);
  assert_eq!(frame.header.request, RequestId::default());
  assert_eq!(frame.header.flags, Flags::default());
  assert_eq!(Sample::from_bytes(&frame.body), Ok(sample()));
}

fn check_frame_encode(result: Result<Vec<u8>, WireError>) {
  let bytes = require_success(result, "encode the benchmark frame");
  let mut remaining = bytes.as_slice();
  let frame = require_success(framer().decode(&mut remaining), "decode the encoded frame");
  check_frame((frame, remaining.len()));
}

#[library_benchmark(teardown = check_frame_encode)]
#[bench::frame((framer(), sample()))]
fn frame_encode((framer, sample): (Framer, Sample)) -> Result<Vec<u8>, WireError> {
  count_instructions(move || {
    black_box(framer.encode(
      Class::Metadata,
      3,
      RequestId::default(),
      Flags::default(),
      &sample,
    ))
  })
}

fn framed() -> (Framer, Vec<u8>) {
  let framer = framer();
  let bytes = require_success(
    framer.encode(
      Class::Metadata,
      3,
      RequestId::default(),
      Flags::default(),
      &sample(),
    ),
    "prepare the benchmark frame",
  );
  (framer, bytes)
}

fn check_frame_decode(result: Result<(Frame, usize), WireError>) {
  check_frame(require_success(result, "decode the benchmark frame"));
}

#[library_benchmark(teardown = check_frame_decode)]
#[bench::framed_bytes(framed())]
fn frame_decode((framer, bytes): (Framer, Vec<u8>)) -> Result<(Frame, usize), WireError> {
  count_instructions(move || {
    let mut remaining = bytes.as_slice();
    let frame = framer.decode(&mut remaining)?;
    Ok(black_box((frame, remaining.len())))
  })
}

fn check_crc(checksum: u32) {
  // RFC 3720 Appendix B.4's reflected Castagnoli polynomial. This bit-at-a-time oracle
  // shares neither the production tables nor its hardware intrinsics.
  let mut expected = u32::MAX;
  for byte in [0xA5u8; 4096] {
    expected ^= u32::from(byte);
    for _bit in 0..u8::BITS {
      expected = (expected >> 1) ^ (0x82F6_3B78 & 0u32.wrapping_sub(expected & 1));
    }
  }
  assert_eq!(checksum, !expected);
}

#[library_benchmark(teardown = check_crc)]
#[bench::page(vec![0xA5u8; 4096])]
fn crc32c_page(bytes: Vec<u8>) -> u32 {
  count_instructions(move || black_box(crc32c(&bytes)))
}

library_benchmark_group!(
  name = wire;
  benchmarks = header_encode, header_decode, body_encode, body_decode, frame_encode, frame_decode, crc32c_page
);

main!(
  config = LibraryBenchmarkConfig::default().tool(
    Callgrind::with_args(["--collect-atstart=no"]).entry_point(EntryPoint::None)
  );
  library_benchmark_groups = wire
);
