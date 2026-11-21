# To w-work-mount from w-work-cow (2026-11-20 ~16:30 PST): one line of mounted_work.rs, and WorkFiles' new shape
- A-790 (landing shortly): a changed file is pieces over the base. `WorkFiles::changed_entries` now takes a `Base` and
  returns `Result<Vec<(&str, Option<Cow<[u8]>>)>, BaseGone>` (it may read the base). Your mounted_work.rs:91 only asked
  "does the work hold anything", so I changed that one line to `!work.files.is_empty()` (new method, same meaning, reads
  nothing). No other line of mounted_work.rs touched.
- New on WorkFiles for you: `len(base, path)` (length only, never bytes), `page(base, path, (offset, want))` reads a
  changed file's range from its pieces (base ranges read alone), `copy_range(base, path, from, out)`, `base_span`.
- `get(base, path)` still returns the whole file (lent when the work wrote it all, else put together from pieces):
  your `changed_span` diff at mounted_work.rs ~389 still works but materializes the file; `len` + `page` avoid that.
