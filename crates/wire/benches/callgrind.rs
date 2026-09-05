//! Instruction-count benches for the wire under callgrind (iai-callgrind): header encode and
//! decode, a sample body's encode and decode, a frame's encode and decode, CRC32C over a page
//! (D-20; BENCHMARKS.md "Ratchets").

// The benchmark macros generate undocumented modules and constants; the doc rule is for our
// own items, which are documented above each function.
#![allow(missing_docs)]

use std::hint::black_box;

use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use slates_wire::crc32c::crc32c;
use slates_wire::frame::KindEntry;
use slates_wire::header::{Class, Flags, Header};
use slates_wire::{FrameCaps, Framer, RequestId, Wire};

#[derive(Wire)]
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

#[library_benchmark]
#[bench::encode(header())]
fn header_encode(header: Header) -> [u8; 32] {
  black_box(header.encode())
}

#[library_benchmark]
#[bench::decode(header().encode())]
fn header_decode(bytes: [u8; 32]) -> Option<Header> {
  black_box(Header::decode(&bytes).ok())
}

#[library_benchmark]
#[bench::body(sample())]
fn body_encode(sample: Sample) -> Vec<u8> {
  black_box(sample.to_bytes())
}

#[library_benchmark]
#[bench::bytes(sample().to_bytes())]
fn body_decode(bytes: Vec<u8>) -> bool {
  black_box(Sample::from_bytes(&bytes).is_ok())
}

#[library_benchmark]
#[bench::frame((framer(), sample()))]
fn frame_encode((framer, sample): (Framer, Sample)) -> Option<Vec<u8>> {
  black_box(
    framer
      .encode(
        Class::Metadata,
        3,
        RequestId::default(),
        Flags::default(),
        &sample,
      )
      .ok(),
  )
}

fn framed() -> (Framer, Vec<u8>) {
  let framer = framer();
  let bytes = framer
    .encode(
      Class::Metadata,
      3,
      RequestId::default(),
      Flags::default(),
      &sample(),
    )
    .unwrap_or_default();
  (framer, bytes)
}

#[library_benchmark]
#[bench::framed_bytes(framed())]
fn frame_decode((framer, bytes): (Framer, Vec<u8>)) -> bool {
  black_box(framer.decode(&mut bytes.as_slice()).is_ok())
}

#[library_benchmark]
#[bench::page(vec![0xA5u8; 4096])]
fn crc32c_page(bytes: Vec<u8>) -> u32 {
  black_box(crc32c(&bytes))
}

library_benchmark_group!(
  name = wire;
  benchmarks = header_encode, header_decode, body_encode, body_decode, frame_encode, frame_decode, crc32c_page
);

main!(library_benchmark_groups = wire);
