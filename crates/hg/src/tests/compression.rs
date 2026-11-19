//! Tests of `compression` (A-598): the bundles stock Mercurial 7.2 compressed, made plain to exactly the uncompressed
//! bundles it wrote of the same repository; hostile streams (every cut, a bit flipped at every place, engines this
//! crate does not read, bytes after a stream, compression bombs, windows claimed past the admission) refused by kind
//! within the admission; and plain bundles compressed by every engine and level and made plain again. Every golden
//! check holds what a stream inflates to, never the compressed bytes: two engines may compress one input two ways.

use super::{
  BZIP2_BLOCK_UNIT, EncodeRefusal, Engine, Refusal, StreamRefusal, ZLIB_WINDOW, Zstd, ZstdFrame,
  bzip2_held, compress, compress_bundle, inflate, plain, run,
};
use crate::bundle2::{self, Bundle2, StreamParam};
use crate::read::to_u64;
use crate::{Admission, PastAdmission, bundle1, fixtures, http};
use proptest::prelude::*;
use std::borrow::Cow;

/// `hg bundle --all --type gzip-v1`: `HG10GZ` and a zlib stream of the changegroup.
const CG01_GZIP: &[u8] = include_bytes!("../../fixtures/compressed/cg01-gzip.hg");
/// `--type bzip2-v1`: `HG10` and a bzip2 stream, whose magic `BZ` completes the header.
const CG01_BZIP2: &[u8] = include_bytes!("../../fixtures/compressed/cg01-bzip2.hg");
/// `--type gzip-v2;cg.version=02;phases=true;obsolescence=true`: `Compression=GZ`.
const CG02_GZIP: &[u8] = include_bytes!("../../fixtures/compressed/cg02-gzip.hg");
/// The same with `bzip2-v2`: `Compression=BZ`.
const CG02_BZIP2: &[u8] = include_bytes!("../../fixtures/compressed/cg02-bzip2.hg");
/// The same with `zstd-v2`: `Compression=ZS`.
const CG02_ZSTD: &[u8] = include_bytes!("../../fixtures/compressed/cg02-zstd.hg");
/// What 7.2's server answered a clone's `getbundle` over HTTP with (A-569's capture): `application/mercurial-0.2`,
/// naming `zstd`, then a bundle2 stream as the wire protocol compressed it.
const WIRE_ZSTD: &[u8] = include_bytes!("../../fixtures/wire/http-clone.03.response-body");

/// Each compressed golden bundle, the engine that compressed it, and the uncompressed bundle 7.2 wrote of the same
/// repository (`make-compressed-fixtures.sh` made both from one repository; its run compared the uncompressed ones
/// with `fixtures/cg01.hg` and `fixtures/cg02.hg`, byte for byte).
const GOLDEN: [(&[u8], Engine, &[u8]); 5] = [
  (CG01_GZIP, Engine::Zlib, fixtures::CG01),
  (CG01_BZIP2, Engine::Bzip2, fixtures::CG01),
  (CG02_GZIP, Engine::Zlib, fixtures::CG02),
  (CG02_BZIP2, Engine::Bzip2, fixtures::CG02),
  (CG02_ZSTD, Engine::Zstd, fixtures::CG02),
];

/// The front of a bundle2 that 7.2 compressed: `HG20`, the stream parameters' length, `Compression=XX`.
const BUNDLE2_FRONT: usize = 22;

/// A bound for hostile input: well above every golden stream's need (a bzip2 block's 3.6 MB of tables, zstd's 2.5 MB
/// of buffers), well below what a claimed window could ask.
const BOUND: u64 = 1 << 26;

fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// A bundle2 of `params` (a raw block) and `rest`.
fn bundle2_with(params: &[u8], rest: &[u8]) -> Vec<u8> {
  let mut out = bundle2::MAGIC.to_vec();
  out.extend_from_slice(&i32::try_from(params.len()).unwrap().to_be_bytes());
  out.extend_from_slice(params);
  out.extend_from_slice(rest);
  out
}

/// The stream of a compressed golden bundle: after `HG10GZ`, after `HG10` (bzip2's own `BZ` is its stream's), or after
/// a bundle2's front.
fn stream_of(bundle: &[u8], engine: Engine) -> &[u8] {
  match (bundle.starts_with(b"HG10"), engine) {
    (true, Engine::Bzip2) => &bundle[4..],
    (true, _) => &bundle[6..],
    (false, _) => &bundle[BUNDLE2_FRONT..],
  }
}

/// What the decoder of `stream` is charged before it inflates a byte.
fn window_of(engine: Engine, stream: &[u8]) -> u64 {
  match engine {
    Engine::Uncompressed => 0,
    Engine::Zlib => ZLIB_WINDOW,
    Engine::Bzip2 => bzip2_held(stream).unwrap(),
    Engine::Zstd => ZstdFrame::read(stream).unwrap().held(),
  }
}

/// What making a bundle2's stream parameters plain charges: one `StreamParam` and the block's bytes.
fn params_charge(bundle: &[u8]) -> u64 {
  if bundle.starts_with(b"HG10") {
    0
  } else {
    to_u64(core::mem::size_of::<StreamParam>() + "Compression=XX".len())
  }
}

/// Whether `refusal` says the bundle was cut short: its magic, its bundle1 or bundle2 header, or its stream.
fn is_cut(refusal: &Refusal, engine: Engine) -> bool {
  match refusal {
    Refusal::Truncated(_)
    | Refusal::Bundle1(bundle1::Refusal::Truncated(_))
    | Refusal::Bundle2(bundle2::Refusal::Truncated(_)) => true,
    Refusal::Stream(StreamRefusal::Truncated { engine: found, .. }) => *found == engine,
    _ => false,
  }
}

/// AC-HG.1 (golden), A-598: do make plain each bundle stock 7.2 compressed: a bundle1 with gzip and with bzip2, a
/// bundle2 with gzip, bzip2 and zstd. Expect exactly the uncompressed bundle 7.2 wrote of the same repository, byte
/// for byte and owned; and the uncompressed golden bundles back as they were, borrowed, nothing copied.
#[test]
fn the_golden_compressed_bundles_are_made_plain_to_the_bundles_mercurial_wrote_uncompressed() {
  for (bundle, engine, twin) in GOLDEN {
    let made = plain(bundle, &mut room()).unwrap();
    assert!(matches!(made, Cow::Owned(_)), "{engine:?}");
    assert_eq!(made.as_ref(), twin, "{engine:?}");
  }
  for twin in [
    fixtures::CG01,
    fixtures::CG02,
    fixtures::CG03,
    fixtures::CG04,
  ] {
    assert!(matches!(plain(twin, &mut room()), Ok(Cow::Borrowed(same)) if same == twin));
  }
}

/// AC-HG.1 (golden), A-598: do inflate with the zstd engine what 7.2's server compressed for a clone's `getbundle`
/// over HTTP, a stream made by the wire protocol's path rather than a bundle's. Expect its one frame inflated whole
/// into a bundle2 stream that decodes and carries the changegroup.
#[test]
fn the_zstd_engine_inflates_what_mercurials_server_compressed_for_the_wire() {
  let (name, payload) = http::decode_v2(WIRE_ZSTD).unwrap();
  assert_eq!(name, b"zstd");
  let mut out = Vec::new();
  inflate(Engine::Zstd, payload, &mut room(), &mut out).unwrap();
  let stream = Bundle2::decode(&out, &mut room()).unwrap();
  assert!(stream.top_level().any(|(_, part)| part.is(b"changegroup")));
}

/// A-598: do give the decoders the compressed golden bundles as they are. Expect each refused by kind as still
/// compressed, naming its engine: a bundle2 by `Bundle2::decode`, a bundle1 by `bundle1::changegroup`.
#[test]
fn a_bundle_still_compressed_is_refused_by_the_decoders_by_kind() {
  for (bundle, engine) in [
    (CG02_GZIP, Engine::Zlib),
    (CG02_BZIP2, Engine::Bzip2),
    (CG02_ZSTD, Engine::Zstd),
  ] {
    assert_eq!(
      Bundle2::decode(bundle, &mut room()),
      Err(bundle2::Refusal::CompressedStream {
        engine: engine.bundle_type().to_vec()
      })
    );
  }
  for (bundle, engine) in [(CG01_GZIP, *b"GZ"), (CG01_BZIP2, *b"BZ")] {
    assert_eq!(
      bundle1::changegroup(bundle),
      Err(bundle1::Refusal::Compressed { engine })
    );
  }
}

/// AC-HG.9 (hostile input), A-598: do make plain every proper prefix of each compressed golden bundle, from none of
/// its bytes to all but its last. Expect each refused as cut short (its magic, its header or its stream) and never a
/// plain bundle.
#[test]
fn every_cut_of_a_compressed_bundle_is_refused_as_cut_short() {
  for (bundle, engine, _) in GOLDEN {
    for len in 0..bundle.len() {
      let refusal = plain(&bundle[..len], &mut Admission::new(BOUND)).unwrap_err();
      assert!(
        is_cut(&refusal, engine),
        "{engine:?} cut at {len} of {}: {refusal:?}",
        bundle.len()
      );
    }
  }
}

/// AC-HG.9 (hostile input), A-598: do flip one bit of every byte of each compressed golden bundle (the bit its
/// position names, so every bit place is met), and make each plain. Expect never a panic and never past the
/// admission: a typed refusal, or a plain bundle. zlib and bzip2 carry checksums over what they inflate, so a stream
/// of theirs made plain is the bundle 7.2 wrote (a flip that changed only what the bytes never needed, as a case bit
/// of `Compression`); a zstd frame as 7.2 writes it carries none, so a flip there may inflate other bytes, which the
/// bundle2 decoder then judges, again without a panic.
#[test]
fn a_bit_flipped_anywhere_in_a_compressed_bundle_is_refused_or_judged_never_a_panic() {
  let mut refused = 0;
  for (bundle, engine, twin) in GOLDEN {
    for at in 0..bundle.len() {
      let mut flipped = bundle.to_vec();
      flipped[at] ^= 1 << (at % 8);
      match plain(&flipped, &mut Admission::new(BOUND)) {
        Err(_) => refused += 1,
        // Judged, then let go: what is tested is that judging it never panics.
        Ok(made) if engine == Engine::Zstd => drop(Bundle2::decode(&made, &mut room())),
        Ok(made) => assert_eq!(made.as_ref(), twin, "{engine:?} flip at {at}"),
      }
    }
  }
  assert!(refused > 0);
}

/// AC-HG.9 (hostile input), A-598: do give bundles naming an engine this crate does not read: a bundle2's
/// `Compression` of `XZ`, of `gz` (7.2 matches bundle types exactly), of `_truncatedBZ` (7.2's internal name, which its
/// reader takes and no writer makes) and with no value; a bundle1 `HG10ZS` (7.2's reader takes it; its writer refuses
/// zstd for bundle1) and `HG10XX`; and magics neither bundle's. Expect each refused by kind before anything is
/// inflated.
#[test]
fn an_engine_this_crate_does_not_read_is_refused_by_kind() {
  let body = stream_of(CG02_GZIP, Engine::Zlib);
  for value in [&b"XZ"[..], b"gz", b"_truncatedBZ"] {
    let bundle = bundle2_with(&[&b"Compression="[..], value].concat(), body);
    assert_eq!(
      plain(&bundle, &mut room()),
      Err(Refusal::Bundle2(bundle2::Refusal::UnknownCompression {
        engine: Some(value.to_vec())
      }))
    );
  }
  assert_eq!(
    plain(&bundle2_with(b"Compression", body), &mut room()),
    Err(Refusal::Bundle2(bundle2::Refusal::UnknownCompression {
      engine: None
    }))
  );
  for marker in [*b"ZS", *b"XX"] {
    let bundle = [&bundle1::MAGIC[..], &marker, body].concat();
    assert_eq!(
      plain(&bundle, &mut room()),
      Err(Refusal::Bundle1(bundle1::Refusal::UnknownCompression {
        engine: marker
      }))
    );
  }
  assert_eq!(
    plain(b"HG30xxxx", &mut room()),
    Err(Refusal::ForeignMagic { found: *b"HG30" })
  );
  assert!(matches!(
    plain(b"HG2", &mut room()),
    Err(Refusal::Truncated(_))
  ));
}

/// A-598: do give a bundle2 that names `Compression` twice, in two cases, to `plain` and to `Bundle2::decode` (7.2
/// takes the last, so two readers could inflate it two ways). Expect both refused `RepeatedCompression`.
#[test]
fn a_bundle2_naming_compression_twice_is_refused() {
  let bundle = bundle2_with(
    b"Compression=GZ compression=BZ",
    stream_of(CG02_GZIP, Engine::Zlib),
  );
  assert_eq!(
    plain(&bundle, &mut room()),
    Err(Refusal::Bundle2(bundle2::Refusal::RepeatedCompression))
  );
  let twice = bundle2_with(b"Compression=UN COMPRESSION=UN", &0_i32.to_be_bytes());
  assert_eq!(
    Bundle2::decode(&twice, &mut room()),
    Err(bundle2::Refusal::RepeatedCompression)
  );
}

/// AC-HG.9 (hostile input), A-598: do append one byte after the stream of each compressed golden bundle (7.2 stops
/// reading at the bundle's end and never sees it). Expect each refused `TrailingBytes` naming one byte.
#[test]
fn a_byte_after_a_compressed_stream_is_refused() {
  for (bundle, engine, _) in GOLDEN {
    let longer = [bundle, b"x"].concat();
    assert_eq!(
      plain(&longer, &mut room()),
      Err(Refusal::Stream(StreamRefusal::TrailingBytes {
        engine,
        bytes: 1
      })),
      "{engine:?}"
    );
  }
}

/// AC-HG.9, A-598 (compression bombs): do compress 8 MiB of zeros with each engine, a stream of a few kilobytes at
/// most, and inflate it within an admission of its decoder's window and 1 MiB. Expect each refused `PastAdmission` by
/// kind with nothing left of the admission, and the inflated bytes never held past it: the room reserved for them is
/// what the admission granted, 1 MiB, however much more the stream claims.
#[test]
fn a_compression_bomb_is_refused_within_its_admission() {
  let zeros = vec![0_u8; 8 << 20];
  let granted = 1 << 20;
  for (engine, level) in [(Engine::Zlib, 6), (Engine::Bzip2, 1), (Engine::Zstd, 3)] {
    let mut stream = Vec::new();
    compress(engine, level, &zeros, &mut stream).unwrap();
    assert!(stream.len() < zeros.len() / 100, "{engine:?}");
    let window = window_of(engine, &stream);
    let mut admission = Admission::new(window + granted);
    let mut out = Vec::new();
    let refusal = inflate(engine, &stream, &mut admission, &mut out).unwrap_err();
    assert!(
      matches!(
        refusal,
        StreamRefusal::PastAdmission { engine: found, past: PastAdmission { left: 0, .. }, .. } if found == engine
      ),
      "{engine:?}: {refusal:?}"
    );
    assert_eq!(admission.left(), 0);
    assert_eq!(to_u64(out.capacity()), granted, "{engine:?}");
    assert!(out.iter().all(|byte| *byte == 0));
  }
}

/// AC-HG.9, A-598 (a window claimed past the admission): do give zstd frames whose headers claim 2 GiB and 2 TiB
/// windows over a few bytes, and a bzip2 stream naming 900,000-byte blocks, each within 1 MiB. Expect each refused
/// `PastAdmission` with nothing inflated, before its decoder is made.
#[test]
fn a_window_past_the_admission_is_refused_before_a_decoder_is_made() {
  let granted = 1 << 20;
  // Magic, a descriptor with no flags, a Window_Descriptor of Exponent 21 (2^31) and 31 (2^41), a raw block of 1.
  for descriptor in [21_u8 << 3, 31 << 3] {
    let frame = [
      &[0x28_u8, 0xB5, 0x2F, 0xFD, 0x00, descriptor][..],
      &[0x09_u8, 0x00, 0x00, 0x61][..],
    ]
    .concat();
    let refusal = inflate(
      Engine::Zstd,
      &frame,
      &mut Admission::new(granted),
      &mut Vec::new(),
    );
    let Err(StreamRefusal::PastAdmission {
      engine,
      inflated,
      past,
    }) = refusal
    else {
      panic!("{refusal:?}");
    };
    assert_eq!((engine, inflated, past.left), (Engine::Zstd, 0, granted));
    assert!(past.asked > 1 << 31);
  }
  let refusal = inflate(
    Engine::Bzip2,
    b"BZh91AY&SY",
    &mut Admission::new(granted),
    &mut Vec::new(),
  );
  assert_eq!(
    refusal,
    Err(StreamRefusal::PastAdmission {
      engine: Engine::Bzip2,
      inflated: 0,
      past: PastAdmission {
        asked: 9 * BZIP2_BLOCK_UNIT * 4,
        left: granted
      }
    })
  );
}

/// A-598 (the admission's edge): do make plain each compressed golden bundle within exactly what it needs (its
/// stream parameters, its decoder's window, its header and its inflated bytes) and within one byte less. Expect the
/// first made plain with nothing of the admission left, and the second refused `PastAdmission`.
#[test]
fn exactly_what_a_bundle_needs_makes_it_plain_and_a_byte_less_refuses_it() {
  for (bundle, engine, twin) in GOLDEN {
    let need =
      params_charge(bundle) + window_of(engine, stream_of(bundle, engine)) + to_u64(twin.len());
    let mut admission = Admission::new(need);
    assert_eq!(
      plain(bundle, &mut admission).unwrap().as_ref(),
      twin,
      "{engine:?}"
    );
    assert_eq!(admission.left(), 0, "{engine:?}");
    let short = plain(bundle, &mut Admission::new(need - 1));
    assert!(
      matches!(
        short,
        Err(Refusal::Stream(StreamRefusal::PastAdmission { engine: found, .. })) if found == engine
      ),
      "{engine:?}: {short:?}"
    );
  }
}

/// A-598 (the charge is what libzstd holds): do inflate the golden zstd frame through the decoder `inflate` makes, and
/// ask libzstd its context's size (`ZSTD_sizeof_DCtx`) before and after. Expect what it holds beyond its own context
/// to be exactly what the frame's header was charged: its input and output buffers for 7.2's 2 MiB window, 2,490,432
/// bytes.
#[test]
fn libzstd_holds_exactly_what_a_frame_is_charged() {
  let stream = stream_of(CG02_ZSTD, Engine::Zstd);
  let frame = ZstdFrame::read(stream).unwrap();
  assert_eq!(frame.window, 1 << 21);
  assert_eq!(frame.content_size, None);
  assert_eq!(frame.held(), 2_490_432);
  let mut admission = room();
  let mut decoder = Zstd::new(stream, &mut admission).unwrap();
  let context = decoder.0.sizeof();
  let mut out = Vec::new();
  run(&mut decoder, stream, &mut admission, &mut out).unwrap();
  assert_eq!(to_u64(decoder.0.sizeof() - context), frame.held());
  assert_eq!(out.as_slice(), &fixtures::CG02[8..]);
}

/// A-598 (round trip): do compress the uncompressed golden bundles with every engine their kind takes, at each
/// engine's lowest and highest level and at 7.2's default (zlib 6, bzip2 9, zstd 3), then make each plain. Expect each
/// framed as 7.2 frames it (`HG10GZ`; `HG10` and bzip2's `BZ`; `HG20` with `Compression=<type>` its one stream
/// parameter, 7.2's front byte for byte) and made plain to exactly the bundle it was made of.
#[test]
fn a_plain_bundle_compressed_by_every_engine_and_level_is_made_plain_to_itself() {
  let zstd_levels = zstd::zstd_safe::min_c_level()..=zstd::zstd_safe::max_c_level();
  let cases = [
    (fixtures::CG01, Engine::Zlib, vec![0, 6, 9], &b"HG10GZ"[..]),
    (fixtures::CG01, Engine::Bzip2, vec![1, 9], &b"HG10BZ"[..]),
    (
      fixtures::CG01,
      Engine::Uncompressed,
      vec![0],
      &b"HG10UN"[..],
    ),
    (
      fixtures::CG02,
      Engine::Zlib,
      vec![0, 6, 9],
      &CG02_GZIP[..BUNDLE2_FRONT],
    ),
    (
      fixtures::CG02,
      Engine::Bzip2,
      vec![1, 9],
      &CG02_BZIP2[..BUNDLE2_FRONT],
    ),
    (
      fixtures::CG02,
      Engine::Zstd,
      vec![*zstd_levels.start(), 3, *zstd_levels.end()],
      &CG02_ZSTD[..BUNDLE2_FRONT],
    ),
    (
      fixtures::CG02,
      Engine::Uncompressed,
      vec![0],
      &fixtures::CG02[..8],
    ),
  ];
  for (twin, engine, levels, front) in cases {
    for level in levels {
      let mut out = Vec::new();
      compress_bundle(twin, engine, level, &mut out).unwrap();
      assert!(out.starts_with(front), "{engine:?} at {level}");
      assert_eq!(
        plain(&out, &mut room()).unwrap().as_ref(),
        twin,
        "{engine:?} at {level}"
      );
    }
  }
}

/// A-598: do compress at a level outside each engine's range (7.2's zlib `-1` among them, which a caller names as 6),
/// zstd into a bundle1, and bundles that are not plain. Expect each refused by kind with nothing written.
#[test]
fn levels_and_bundles_a_compressor_does_not_take_are_refused_with_nothing_written() {
  let zstd_past = zstd::zstd_safe::max_c_level() + 1;
  for (engine, level) in [
    (Engine::Zlib, -1),
    (Engine::Zlib, 10),
    (Engine::Bzip2, 0),
    (Engine::Bzip2, 10),
    (Engine::Zstd, zstd_past),
    (Engine::Uncompressed, 1),
  ] {
    let mut out = Vec::new();
    assert_eq!(
      compress(engine, level, b"abc", &mut out),
      Err(EncodeRefusal::Level { engine, level })
    );
    assert_eq!(
      compress_bundle(fixtures::CG02, engine, level, &mut out),
      Err(EncodeRefusal::Level { engine, level })
    );
    assert!(out.is_empty());
  }
  let mut out = Vec::new();
  assert_eq!(
    compress_bundle(fixtures::CG01, Engine::Zstd, 3, &mut out),
    Err(EncodeRefusal::NotForBundle1)
  );
  for bundle in [CG01_GZIP, CG02_ZSTD, b"HG30", b"HG2"] {
    assert!(matches!(
      compress_bundle(bundle, Engine::Zlib, 6, &mut out),
      Err(EncodeRefusal::NotPlain { .. })
    ));
  }
  assert!(out.is_empty());
}

/// A plain bundle2 of the advisory stream parameter `note` and `rest` after its parameters.
fn noted(note: &[u8], rest: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  Bundle2 {
    params: vec![StreamParam {
      name: b"note".to_vec(),
      value: Some(note.to_vec()),
    }],
    parts: Vec::new(),
    events: Vec::new(),
  }
  .encode(&mut out)
  .unwrap();
  out.truncate(out.len() - 4);
  out.extend_from_slice(rest);
  out
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-598 (round trip): do compress arbitrary changegroup bytes as a bundle1, and arbitrary bytes after an advisory
  /// stream parameter as a bundle2, with every engine each kind takes at 7.2's default level, then make each plain.
  /// Expect the compressed bundle other than the plain one (its header names an engine), and exactly the plain bundle
  /// back, its advisory parameter kept and `Compression` dropped.
  #[test]
  fn arbitrary_bundles_round_trip_through_every_engine(
    body in proptest::collection::vec(any::<u8>(), 0..2048),
    note in proptest::collection::vec(any::<u8>(), 0..16),
  ) {
    let as_bundle1 = [&bundle1::MAGIC[..], &bundle1::UNCOMPRESSED, body.as_slice()].concat();
    let as_bundle2 = noted(&note, &body);
    for (bundle, engine, level) in [
      (&as_bundle1, Engine::Zlib, 6),
      (&as_bundle1, Engine::Bzip2, 9),
      (&as_bundle2, Engine::Zlib, 6),
      (&as_bundle2, Engine::Bzip2, 9),
      (&as_bundle2, Engine::Zstd, 3),
    ] {
      let mut out = Vec::new();
      compress_bundle(bundle, engine, level, &mut out).unwrap();
      prop_assert_ne!(&out, bundle);
      let made = plain(&out, &mut room()).unwrap();
      prop_assert_eq!(made.as_ref(), bundle.as_slice());
    }
  }

  /// AC-HG.9 (hostile input), A-598: do make plain arbitrary bytes after each engine's header (zlib's, bzip2's at
  /// level 9, a zstd frame's with 7.2's 2 MiB window), in a bundle1 and a bundle2. Expect a typed refusal or a plain
  /// bundle inflated from them, never the compressed bytes handed back as plain, never a panic, within a bounded
  /// admission.
  #[test]
  fn arbitrary_bytes_after_a_compressed_header_never_panic(
    bytes in proptest::collection::vec(any::<u8>(), 0..512),
  ) {
    let zlib = [&b"x\x9c"[..], bytes.as_slice()].concat();
    let bzip2 = [&b"BZh9"[..], bytes.as_slice()].concat();
    let zstd_frame = [&[0x28_u8, 0xB5, 0x2F, 0xFD, 0x00, 0x58][..], bytes.as_slice()].concat();
    let inputs = [
      [&b"HG10GZ"[..], zlib.as_slice()].concat(),
      [&b"HG10"[..], bzip2.as_slice()].concat(),
      bundle2_with(b"Compression=GZ", &zlib),
      bundle2_with(b"Compression=BZ", &bzip2),
      bundle2_with(b"Compression=ZS", &zstd_frame),
    ];
    for input in inputs {
      match plain(&input, &mut Admission::new(BOUND)) {
        Ok(made) => prop_assert!(matches!(made, Cow::Owned(_))),
        Err(refusal) => {
          let foreign = matches!(refusal, Refusal::ForeignMagic { .. });
          prop_assert!(!foreign);
        }
      }
    }
  }
}
