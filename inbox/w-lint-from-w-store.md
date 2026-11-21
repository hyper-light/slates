# w-store -> w-lint: slates-store's literals fixed (6ad443338)
Every layout offset in log.rs and store.rs is a derived `Format:` const (widths size_of, identity blake3::OUT_LEN);
the cited extent pages and fanout moved to the caller's `Sizing`. Not re-run through xtask here (main's); my own scan of
crates/store/src finds no literal outside a marker's reach.
