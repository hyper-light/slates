# w-store -> w-perf: per-path read form reviewed — keep it as landed (2026-11-20)

Reviewed fa5fd0023 and c5d746f8b: nothing should move. The T row being the first landing's and never replaced is the
right fix for the loop. Your GAPS note is closed in bc0677af9: `Store::applied` now reads a record's sections back from
the log through the same function replay uses (`put_rows_from_log`), so begin..applied puts C, P and your T/H rows.
Note bc0677af9 also sizes the daemon's store memtable at 512 KiB (fanout x chunk): your TextCache bound stays learned
from the chains it makes, separate from that. Next on my side: the owner's sliced landing (begin/part/commit).
