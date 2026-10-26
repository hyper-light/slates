# A destroyed volume's Shards stream was ended again on each teardown pass (2026-10-26)

## Description

Destroying a volume that a Shards stream serves counted `shards.end_refused` once for every pass the destroy's
teardown waited for the stream, although the stream ended correctly: the guest read the end of its stream, and the
attachment's record was removed. Found by a new test, `a_destroy_ends_the_stream_and_removes_its_record`
(`crates/client/tests/shards.rs`, written for A-179, step 1 of G-shards-windows), which asserts no end is refused:
on macOS it failed with `{"shards.end_refused": 2, ...}`.

## Root cause

Confirmed by a log line in `shards::end_stream` printing whether the stream was already ending and its callers:

```
end_stream already_ending=false  <- end_volume_streams <- verbs::destroy
end_stream already_ending=true   <- end_volume_streams <- verbs::teardown_started <- step_destroys
end_stream already_ending=true   <- end_attachment <- verbs::detach
end_stream already_ending=true   <- end_volume_streams <- verbs::teardown_started <- step_destroys
```

The destroy ends the stream. Then `verbs::teardown_started` asks `end_volume_streams` again each pass while it waits
for the stream to finish serving, and `end_volume_streams` ended every stream of the volume again, ending or not. A
second `shutdown(Both)` of a socket is refused on macOS (`ENOTCONN`), which `end_stream` counts as a failed end.
Linux accepts a second shutdown, so it never showed there. On Windows the second end would disconnect a pipe already
disconnected and cancel nothing.

## Impact

None on data or on the attachment's ending: the first end did its work, and the task ended the attachment at its next
turn. The refusal ledger misreported a failed end per teardown pass, so a real failure to end a stream could not be
told from the noise. No test asserted the counter after a destroy before this one.

## Exact edits

`crates/server/src/shards.rs`, `end_stream`: a stream already marked `ending` is left as it is, since its task ends it
at its next turn. The offer withdrawal before it still runs every time (it is idempotent). The test above passes after
the edit, and the server's Shards suite (26 tests) is unchanged.
