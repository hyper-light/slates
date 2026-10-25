# A direct map's read past its end was refused

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record (with A-172).
- **Subsystem:** the client's mapped read, `MappedRead::read_at` in `crates/client/src/client.rs`, over
  `slates_mem::PeerView::load` (§4.6, A-171).
- **Severity:** an error where a read should return nothing. No wrong bytes were ever returned.
- **Exposure:** a caller of `read_at` on a zero-copy map with an offset past the map's end (Linux, since A-171 made
  maps zero-copy there that morning). The copy path answered 0, as `pread` past a file's end does.
- **Found by:** reading the client while reviewing step 1, then a failing test.

## Symptom

`crates/client/tests/client.rs`, `a_read_from_a_maps_end_or_past_it_returns_nothing`, on fedora before the fix:

```
called `Result::unwrap()` on an `Err` value: Ipc(OsRefused { call: "load a mapped read", code: None })
```

## Root cause

`read_at` clips what it takes to the map's length, so past the end it takes nothing, and then still asked the view
for an empty load at that offset. `PeerView::load` range-checks the load's end against the view, and an empty load
starting past the end ends past it, so it refused `OutOfRange`, which the client reported as an error.

## Fix

`read_at` returns 0 when there is nothing to take (an empty buffer, or an offset at or past the map's end), before
touching the view.

## Verification

The test above fails before the fix on Linux and passes after; it passed both ways on macOS, where the read is a copy.
