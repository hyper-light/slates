use super::{Band, MAX_BAND_DATA, Refusal, demux, write, written_len};
use crate::fixtures::{FETCH_PROGRESS_RESPONSE, FETCH_URIS_RESPONSE, PACKFILE_RESPONSES};
use crate::out::Measure;
use crate::pktline::{self, MAX_LEN, Packet};
use crate::{Admission, to_u64};

/// Format: a pack's first eight bytes, its signature and version 2 (gitformat-pack "pack-*.pack files have the
/// following format").
const PACK_HEAD: &[u8] = b"PACK\0\0\0\x02";

/// The packets of a stream stock git wrote, decoded with room for one packet a byte.
fn packets(stream: &[u8]) -> Vec<Packet<'_>> {
  let room = to_u64(
    stream
      .len()
      .saturating_mul(core::mem::size_of::<Packet<'_>>()),
  );
  pktline::decode(stream, &mut Admission::new(room)).unwrap()
}

/// What each band carried in the packfile section of `stream`: every data packet after its `packfile` line until its
/// flush, demultiplexed, band 1's bytes, band 2's and band 3's.
fn packfile_section(stream: &[u8]) -> [Vec<u8>; 3] {
  let packets = packets(stream);
  let start = packets
    .iter()
    .position(|packet| *packet == Packet::Data(b"packfile\n"))
    .expect("a packfile section");
  let mut bands = [Vec::new(), Vec::new(), Vec::new()];
  for packet in &packets[start + 1..] {
    match packet {
      Packet::Data(data) => {
        let (band, bytes) = demux(data).unwrap();
        bands[usize::from(band.byte()) - 1].extend_from_slice(bytes);
      }
      Packet::Flush => return bands,
      other => panic!("{other:?} inside the packfile section"),
    }
  }
  panic!("the packfile section ends in a flush");
}

/// A-582, golden vectors from stock git 2.50.1. Do: demultiplex the packfile section of each fetch answer. Expect: every
/// data packet after the `packfile` line on band 1 or 2, up to the flush; band 1's bytes one pack (`PACK`, version 2,
/// then its entries and a 20-byte trailer); band 2 empty where the request said `no-progress`, git's progress text where
/// it did not; band 3 never.
#[test]
fn each_fetch_answers_packfile_section_demultiplexes_to_one_pack_and_its_progress() {
  for (name, stream) in PACKFILE_RESPONSES {
    let [pack, progress, error] = packfile_section(stream);
    assert!(
      pack.starts_with(PACK_HEAD),
      "{name}: {:?}",
      &pack[..PACK_HEAD.len().min(pack.len())]
    );
    assert!(
      pack.len() > PACK_HEAD.len() + 4 + 20,
      "{name}: a pack of {} bytes",
      pack.len()
    );
    assert!(
      error.is_empty(),
      "{name}: band 3 said {:?}",
      String::from_utf8_lossy(&error)
    );
    if stream == FETCH_PROGRESS_RESPONSE {
      let text = String::from_utf8(progress).unwrap();
      assert!(text.contains("Total "), "{name}: {text:?}");
    } else {
      assert!(progress.is_empty(), "{name}: progress after no-progress");
    }
  }
}

/// A-582, golden vectors: protocol v2's `sideband-all` (gitprotocol-v2 "fetch"). Do: walk the answer stock git sent to a
/// `sideband-all` request with `packfile-uris https`. Expect: every data packet on band 1, the section names
/// (`packfile-uris`, `packfile`) and the URI line among them; the delimiter between the sections and the final flush
/// plain packets; and the bytes after the `packfile` line one pack.
#[test]
fn under_sideband_all_every_data_packet_of_the_answer_is_on_band_one() {
  let packets = packets(FETCH_URIS_RESPONSE);
  let (lines, pack) = lines_and_pack_all_on_band_one(&packets);
  assert_eq!(
    lines.first().map(Vec::as_slice),
    Some(&b"packfile-uris\n"[..])
  );
  let uri = String::from_utf8(lines[1].clone()).unwrap();
  assert!(uri.contains(" https://example.invalid/"), "{uri:?}");
  assert_eq!(lines.last().map(Vec::as_slice), Some(&b"packfile\n"[..]));
  assert!(pack.starts_with(PACK_HEAD));
  assert_eq!(
    packets
      .iter()
      .filter(|packet| **packet == Packet::Delim)
      .count(),
    1
  );
  assert_eq!(packets.last(), Some(&Packet::Flush));
}

/// The text lines of a `sideband-all` answer's `packets` through its `packfile` line, and the bytes after it, holding
/// every data packet to band 1 and no packet to a response end.
fn lines_and_pack_all_on_band_one(packets: &[Packet<'_>]) -> (Vec<Vec<u8>>, Vec<u8>) {
  let mut lines = Vec::new();
  let mut pack = Vec::new();
  let mut in_pack = false;
  for packet in packets {
    match packet {
      Packet::Data(data) => {
        let (band, bytes) = demux(data).unwrap();
        assert_eq!(band, Band::Data, "{bytes:?}");
        if in_pack {
          pack.extend_from_slice(bytes);
        } else {
          in_pack = bytes == b"packfile\n";
          lines.push(bytes.to_vec());
        }
      }
      Packet::Delim | Packet::Flush => {}
      Packet::ResponseEnd => panic!("a response end in a stateless answer's body"),
    }
  }
  (lines, pack)
}

/// A-582: do write a pack stock git sent, on each band, and bytes at, below and past one packet's capacity, measuring
/// each write first. Expect: every packet at most 65520 bytes, the least number of packets the cap allows, the band
/// demultiplexing back to the same bytes, the measure equal to the bytes written, and no packet for no bytes.
#[test]
fn bytes_written_on_a_band_demultiplex_back_in_as_few_packets_as_the_cap_allows() {
  let [pack, _, _] = packfile_section(PACKFILE_RESPONSES[0].1);
  let lengths = [
    0,
    1,
    MAX_BAND_DATA - 1,
    MAX_BAND_DATA,
    MAX_BAND_DATA + 1,
    3 * MAX_BAND_DATA + 7,
  ];
  let mut cases: Vec<Vec<u8>> = lengths
    .iter()
    .map(|len| (0..*len).map(|at: usize| at.to_le_bytes()[0]).collect())
    .collect();
  cases.push(pack);
  for bytes in &cases {
    for band in [Band::Data, Band::Progress, Band::Error] {
      let mut measure = Measure::default();
      write(band, bytes, &mut measure);
      let mut wire = Vec::new();
      write(band, bytes, &mut wire);
      assert_eq!(measure.0, to_u64(wire.len()));
      assert_eq!(written_len(to_u64(bytes.len())), to_u64(wire.len()));
      let packets = packets(&wire);
      assert_eq!(
        packets.len(),
        bytes.len().div_ceil(MAX_BAND_DATA),
        "{} bytes",
        bytes.len()
      );
      let mut back = Vec::new();
      for packet in &packets {
        let Packet::Data(data) = packet else {
          panic!("{packet:?} written on a band");
        };
        assert!(data.len() + 4 <= MAX_LEN);
        let (written_band, payload) = demux(data).unwrap();
        assert_eq!(written_band, band);
        back.extend_from_slice(payload);
      }
      assert_eq!(&back, bytes);
    }
  }
}

/// A-582 hostile input: do demultiplex a packet with no band byte, and one with each byte that names no band. Expect:
/// `NoBand`, and `UnknownBand` naming the byte for every byte but 1, 2 and 3 (git's "bad band"), which read as their
/// bands with the bytes after them.
#[test]
fn a_packet_without_a_band_or_with_a_band_git_does_not_know_is_refused() {
  assert_eq!(demux(b""), Err(Refusal::NoBand));
  for byte in 0..=u8::MAX {
    let data = [byte, b'x'];
    match Band::of(byte) {
      Some(band) => assert_eq!(demux(&data), Ok((band, &b"x"[..]))),
      None => assert_eq!(demux(&data), Err(Refusal::UnknownBand { byte })),
    }
  }
  assert_eq!(
    [Band::Data, Band::Progress, Band::Error].map(Band::byte),
    [1, 2, 3]
  );
}
