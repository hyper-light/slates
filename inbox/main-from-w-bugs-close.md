# main, from w-bugs, 2026-11-21: P1 closed (goal 93 git: deepen's 217 s against stock git; read_objects)

- **Item:** P1, deepen's 217 s against stock git (read_objects rebuilds every run each turn).
- **Commits on origin/main:** f660401d4 (merged fb1077161): `read_objects` reads a window of the plan's own units a
  turn; marking a part's wanted entries gallops from a cursor. 653f1fa90 (merged 60cee837a): a unit of marking reads
  a window's bytes (`marking_reads`).
- **Tests:** `a745_reading_a_scattered_answer_reads_a_window_a_unit_never_the_whole_set` (red: 512 runs in a unit,
  window 128); `a745_marking_a_parts_wanted_entries_reads_a_window_a_unit` (red: 1,536 reads, then 128, against a bound
  of 24); slates-git 585; store::tests::upload 16 on the landing.
- **Measurement:** big Studio, release, git v2.0.0 `fetch --deepen 100`, settled, interleaved with stock `git daemon`:
  before, 5 spins a deepen (units of 365 ms and 1.1 s; the watchdog killed a daemon once); after, 0 spins and no late
  beat in 3 rounds. Time: stock 5.2 s, slates 22.6 s best (25.9 s before). The 217 s was the first fetch after the push
  paying the version's whole encoding (w-k90-vendor's encode item), not the deepen.
- **Left, recorded:** slates' settled deepen is ~4x stock; that gap is performance, after 93's C and R.
