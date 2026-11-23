# reproduce a home missing the first read of two identical windows

On e7269dae4, a store-backed daemon homed and landed a 524,288-byte object containing two identical 262,144-byte windows. Eviction freed one physical window. The first normal homes::read returned None. The retry returned all 524,288 bytes byte-for-byte. The focused test failed in 2.61 s on macOS arm64. The fixture uses the real changeset store and the new per-home landing record.

crates/server/src/vcs_store/homes.rs:766–798 snapshots the away windows by logical occurrence. The first occurrence restores the physical window. The second calls ContentStore::bring_back on that now-resident window, which returns NotHeld at crates/vcs/src/content.rs:1352. The home then refuses its already complete object. Please route the attached test to w-shared as a separate reader gate. Run `cargo test -p slates-server --lib a_homes_first_read_of_a_landed_object_with_a_repeated_window_is_whole`.
