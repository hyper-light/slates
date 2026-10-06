//! Real inter-region paths for the timed simulation (`support::timed`): Microsoft's published P50 round trips
//! among five Azure regions ("Azure network round-trip latency statistics",
//! learn.microsoft.com/en-us/azure/networking/azure-network-latency, page dated 2026-07-30, fetched
//! 2026-09-28; directional, one way taken as half the round trip), and a seeded placement of the regions on a
//! group's hosts.

// Test code may panic (CLAUDE.md §2 item 6 applies to shipped code).
#![allow(clippy::indexing_slicing)]
use slates_db::register::HostId;

use super::timed::{MS, Profile};

/// Format: the regions, in the matrix's order.
pub(crate) const REGIONS: [&str; 5] = [
  "East US",
  "West Europe",
  "Japan East",
  "Southeast Asia",
  "Brazil South",
];
/// Format: the published P50 round trips, in milliseconds, source row to destination column, among
/// [`REGIONS`] (read from the page's tables; `East US` → `West Europe` is 83 ms and `West Europe` →
/// `East US` 85 ms — the page is directional).
pub(crate) const ROUND_TRIPS_MS: [[u64; 5]; 5] = [
  [0, 83, 162, 224, 117],
  [85, 0, 233, 169, 185],
  [162, 234, 0, 72, 262],
  [224, 169, 72, 0, 330],
  [118, 185, 262, 331, 0],
];

/// Which host sits in each of the first `regions` regions under `seed`: a permutation of the hosts
/// `1..=regions` (Fisher–Yates over splitmix64). The simulation numbers its voters `1..=n` and the
/// election timer's jitter is a draw from the node's id, so with a fixed placement the same region would win
/// every first election — one region led all twenty seeds before this (2026-09-28); a fleet's member ids are
/// random 64-bit values, which the permutation models.
pub(crate) fn placement(regions: usize, seed: u64) -> Vec<HostId> {
  let mut hosts: Vec<HostId> = (1..=u64::try_from(regions).unwrap()).map(HostId).collect();
  let mut state = seed;
  for index in (1..regions).rev() {
    state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut mixed = state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^= mixed >> 31;
    let pick = usize::try_from(mixed % u64::try_from(index + 1).unwrap()).unwrap();
    hosts.swap(index, pick);
  }
  hosts
}

/// The profile of the regions on `hosts` (region `i` on `hosts[i]`): each direction half the published
/// round trip from its source, with `jitter_ns` on top and `loss_ppm` of the messages lost.
pub(crate) fn profile(hosts: &[HostId], jitter_ns: u64, loss_ppm: u32) -> Profile {
  let mut profile = Profile::uniform(0, jitter_ns, loss_ppm);
  for (from, from_host) in hosts.iter().enumerate() {
    for (to, to_host) in hosts.iter().enumerate() {
      if from != to {
        let one_way = ROUND_TRIPS_MS[from][to] * MS / 2;
        profile
          .pairs
          .insert((*from_host, *to_host), (one_way, jitter_ns));
      }
    }
  }
  profile
}

/// The published quorum round trip of region `node` among the first `regions`: the `⌊n/2⌋`-th smallest
/// round trip from it to the others.
pub(crate) fn quorum_round_trip_ms(node: usize, regions: usize) -> u64 {
  let mut trips: Vec<u64> = (0..regions)
    .filter(|other| *other != node)
    .map(|other| ROUND_TRIPS_MS[node][other])
    .collect();
  trips.sort_unstable();
  trips[regions / 2 - 1]
}
