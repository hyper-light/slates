//! The Phase 0 wire baseline: header encode and decode, a sample body's encode and decode, a
//! whole frame's encode and decode, and CRC32C throughput.
//!
//! `cargo run --release -p slates-wire --example bench`

use std::time::Duration;

use slates_machine::bench::{Measurement, bytes_per_second, measure};
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

fn report(name: &str, m: Measurement) {
  println!(
    "ratchet\t{}\t{}\t{}\t{}",
    key(name),
    m.interval.lower,
    m.median_ns(),
    m.interval.upper
  );
  println!(
    "{name}: median {} ns [{}, {}] p99 {} ns, {} samples × batch {}{}",
    m.median_ns(),
    m.interval.lower,
    m.interval.upper,
    m.p99_ns,
    m.samples,
    m.batch,
    if m.quick { " (quick)" } else { "" }
  );
}

/// The ratchet key of a row: `wire.` plus the row's name in snake case, whole, so two rows
/// that share their first words stay distinct.
fn key(name: &str) -> String {
  let words: Vec<&str> = name
    .split(|c: char| !c.is_ascii_alphanumeric())
    .filter(|w| !w.is_empty())
    .collect();
  format!("wire.{}", words.join("_").to_ascii_lowercase())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let budget = Duration::from_millis(400);
  let header = Header {
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
  };
  report(
    "header encode (32 bytes)",
    measure(
      || {
        std::hint::black_box(std::hint::black_box(&header).encode());
      },
      budget,
    ),
  );
  let bytes = header.encode();
  report(
    "header decode",
    measure(
      || {
        std::hint::black_box(Header::decode(std::hint::black_box(&bytes)).ok());
      },
      budget,
    ),
  );

  let sample = Sample {
    name: "volume-1234".to_owned(),
    id: [7; 16],
    size: 1 << 30,
    flags: vec![1, 2, 3, 4],
    note: Some("witnessed".to_owned()),
  };
  let body = sample.to_bytes();
  println!("sample body: {} bytes", body.len());
  let mut out = Vec::with_capacity(256);
  report(
    "body encode (sample, into a reused buffer)",
    measure(
      || {
        out.clear();
        sample.encode(&mut out);
        std::hint::black_box(&out);
      },
      budget,
    ),
  );
  report(
    "body decode (sample)",
    measure(
      || {
        std::hint::black_box(Sample::from_bytes(std::hint::black_box(&body)).ok());
      },
      budget,
    ),
  );

  let framer = Framer::new(
    FrameCaps::explicit(1 << 16, 1 << 16, 1 << 20, 1 << 10),
    vec![KindEntry {
      class: Class::Metadata,
      kind: 3,
      schema_hash: Sample::SCHEMA_HASH,
    }],
  );
  report(
    "frame encode (header + schema word + body + CRC32C)",
    measure(
      || {
        std::hint::black_box(
          framer
            .encode(
              Class::Metadata,
              3,
              RequestId::default(),
              Flags::default(),
              &sample,
            )
            .ok(),
        );
      },
      budget,
    ),
  );
  let frame = framer.encode(
    Class::Metadata,
    3,
    RequestId::default(),
    Flags::default(),
    &sample,
  )?;
  report(
    "frame decode (checks, CRC32C, schema word; body bytes copied)",
    measure(
      || {
        std::hint::black_box(
          framer
            .decode(&mut std::hint::black_box(frame.as_slice()))
            .ok(),
        );
      },
      budget,
    ),
  );

  let page = usize::try_from(slates_machine::facts::Facts::query().page.base)?;
  let buf: Vec<u8> = (0..page * 64)
    .map(|i| u8::try_from(i % 251).unwrap_or(0))
    .collect();
  let m = measure(
    || {
      std::hint::black_box(crc32c(&buf));
    },
    budget,
  );
  println!(
    "ratchet\twire.crc32c_1mib\t{}\t{}\t{}",
    m.interval.lower,
    m.median_ns(),
    m.interval.upper
  );
  println!(
    "crc32c over {} bytes: median {} ns, {:.1} GB/s",
    buf.len(),
    m.median_ns(),
    bytes_per_second(u64::try_from(buf.len())?, m.median_ns()) as f64 / 1e9
  );
  Ok(())
}
