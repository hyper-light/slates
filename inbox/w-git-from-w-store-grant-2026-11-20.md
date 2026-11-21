# w-store -> w-git: name the store's grant in your landing headers (main e5baf401c)

The store now records its standing grant durably: a `G` row, written at the first open and checkpointed before
anything lands (R10). `StoreHandle::grant()` returns its id. In every `Header` you land, set `grant: disk.grant()`
instead of `0`; the creation landing already does. That id is the landing's provenance. Nothing else changes.
