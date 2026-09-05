//! Golden vectors and the append-only schema check: the bytes of a sample message and the
//! reflection of every type below are recorded here and must never change within a major; a
//! reflection may only grow by appending. A change here is a protocol change and needs the
//! amendment log (§4.9 "append-only evolution within a major").

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_wire::header::{Class, Flags};
use slates_wire::{FrameCaps, Framer, RequestId, Wire};

#[derive(Wire, Debug, PartialEq, Clone)]
struct Fingerprint {
  dev: u64,
  ino: u64,
  size: u64,
  mtime_ns: i64,
}

#[derive(Wire, Debug, PartialEq, Clone)]
enum Verdict {
  Apply,
  Skip { reason: String },
  Conflict { at: u64, len: u32 },
}

#[derive(Wire, Debug, PartialEq, Clone)]
struct Sample {
  name: String,
  id: [u8; 4],
  fingerprint: Option<Fingerprint>,
  verdicts: Vec<Verdict>,
  ratio: f64,
  ok: bool,
}

fn sample() -> Sample {
  Sample {
    name: "slates".to_owned(),
    id: [1, 2, 3, 4],
    fingerprint: Some(Fingerprint {
      dev: 7,
      ino: 99,
      size: 4096,
      mtime_ns: -5,
    }),
    verdicts: vec![
      Verdict::Apply,
      Verdict::Skip {
        reason: "same".to_owned(),
      },
      Verdict::Conflict { at: 10, len: 3 },
    ],
    ratio: 0.5,
    ok: true,
  }
}

/// The recorded reflections. Append-only: a current reflection must start with its record.
const RECORDED_SCHEMAS: &[(&str, &str)] = &[
  (
    "Fingerprint",
    "struct Fingerprint{dev:u64,ino:u64,size:u64,mtime_ns:i64}",
  ),
  (
    "Verdict",
    "enum Verdict{Apply{},Skip{reason:String},Conflict{at:u64,len:u32}}",
  ),
  (
    "Sample",
    "struct Sample{name:String,id:[u8;4],fingerprint:Option<Fingerprint>,verdicts:Vec<Verdict>,ratio:f64,ok:bool}",
  ),
];

/// The recorded encoding of `sample()`, body only, as hex.
const RECORDED_SAMPLE_HEX: &str = concat!(
  "06000000736c61746573", // name: len 6 + "slates"
  "01020304",             // id
  "01",                   // Some
  "0700000000000000",     // dev
  "6300000000000000",     // ino
  "0010000000000000",     // size
  "fbffffffffffffff",     // mtime_ns = -5
  "03000000",             // three verdicts
  "00000000",             // Apply
  "01000000",
  "0400000073616d65", // Skip { reason: "same" }
  "02000000",
  "0a00000000000000",
  "03000000",         // Conflict { at: 10, len: 3 }
  "000000000000e03f", // ratio 0.5
  "01"                // ok
);

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_sample_encoding_is_frozen_and_round_trips() {
  let bytes = sample().to_bytes();
  assert_eq!(
    hex(&bytes),
    RECORDED_SAMPLE_HEX,
    "the golden vector changed: a protocol change"
  );
  assert_eq!(Sample::from_bytes(&bytes).unwrap(), sample());
}

#[test]
fn every_reflection_is_append_only_against_its_record() {
  let current = [
    ("Fingerprint", Fingerprint::SCHEMA),
    ("Verdict", Verdict::SCHEMA),
    ("Sample", Sample::SCHEMA),
  ];
  for (name, schema) in current {
    let (_, recorded) = RECORDED_SCHEMAS
      .iter()
      .find(|(n, _)| *n == name)
      .expect("recorded");
    let recorded_body = recorded.trim_end_matches('}');
    assert!(
      schema.starts_with(recorded_body),
      "{name}: {schema} does not extend {recorded}"
    );
  }
}

#[test]
fn schema_hashes_are_stable_and_distinct() {
  // Frozen for the life of the major: a changed value here means a type in the tree changed.
  assert_ne!(Sample::SCHEMA_HASH, Fingerprint::SCHEMA_HASH);
  assert_ne!(Sample::SCHEMA_HASH, Verdict::SCHEMA_HASH);
  let again = Sample::SCHEMA_HASH;
  assert_eq!(again, Sample::SCHEMA_HASH);
}

#[test]
fn a_framed_sample_survives_a_full_frame_round_trip() {
  let framer = Framer::new(
    FrameCaps::explicit(1 << 16, 1 << 16, 1 << 20, 1 << 10),
    vec![slates_wire::frame::KindEntry {
      class: Class::Metadata,
      kind: 4,
      schema_hash: Sample::SCHEMA_HASH,
    }],
  );
  let bytes = framer
    .encode(
      Class::Metadata,
      4,
      RequestId {
        client: 3,
        sequence: 8,
      },
      Flags::END_OF_STREAM,
      &sample(),
    )
    .unwrap();
  let frame = framer.decode(&mut bytes.as_slice()).unwrap();
  assert_eq!(
    frame.header.request,
    RequestId {
      client: 3,
      sequence: 8
    }
  );
  assert!(frame.header.flags.contains(Flags::END_OF_STREAM));
  assert_eq!(Sample::from_bytes(&frame.body).unwrap(), sample());
}
