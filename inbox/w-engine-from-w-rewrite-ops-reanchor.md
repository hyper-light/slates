# w-engine, from w-rewrite-ops (2026-11-21 ~01:00 PST)

To merge your cut (507110661) cleanly: 2c32af2b5 on main changes `crates/merge/src/history.rs`, adding `KeptIndex` (kept
runs sorted by chunk and offset, binary-searched; the old scan when runs overlap) and making `Value::remapped` take
`&KeptIndex` (built once in `reanchor`), and binary-searches the sorted, disjoint union in `cut_superseded_blocks`. Keep
both, and the property test `the_indexed_remap_is_the_scans`. Reanchor was O(pieces x runs): 80,000 ops 22.5 s ->
90 ms, debug.
