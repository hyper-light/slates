# main <- w-fed-move: the stale test, and owner asks per client (2026-11-21 ~04:00 PST)

## 1. The stale test: fixed (Slates 564cc74a)
`daemon`'s `the_merge_service_enforces_roles_pins_versions_and_seals_behind_the_barrier`. A-774 says a green lands
"presented and granted as any volume's landing" (R10). So the grant is the barrier, and it does say greens land
behind it. Only the land assertion changed: a green's land is now judged as a landing, and at the test's missing
target it is refused typed (`Refusal::Target`), not `Unsupported` by role. Every other assertion is kept. GAPS has
the one-line note.

## 2. Owner asks per client: the knob is contention (A-808, Slates 10a0318d; commit b0405dca)
Stand-in: 1,000 clients, release, in-process, kernel/. Hot files 300, 100, 30, 10 gave:
- asks per client 17.5, 23.1, 38.7, 69.4;
- rebases per client 2.3, 3.9, 9.7, 22.2.

Counted by the verb that asked (`merge.work.base_ask.<verb>.<kind>`):
- edits re-asked lengths after each rebase: 7.9 to 18.1 a client;
- rebases asked files: 1.6 to 15.0 a client.

### Fixed
An edit now reads its length from the work's kept base entry. A rebase now gives entries for every path the work
touched; before, only for the paths its restated journal named, which dropped any path whose change became the head's.
- Red first: `an_edit_after_a_rebase_reads_its_files_length_from_the_kept_entry`.
- A/B: at 10 hot files, 69.4 to 48.5 asks a client (lengths 18.1 to 4.7, now flat in contention); at 300 hot files,
  17.5 to 14.1. Oracle clean.

### Open, next
Rebases still ask files: 13.8 a client at 10 hot files. These are journal paths the work no longer holds because their
change became identical to the head; the next composition reads their base bytes. Dropping such restated ops at the
rebase would end it, but it changes what a restated journal declares, so I'd like your call on that before changing
it. Also, 4 of 1,000 clients starved past the harness's 64 resolutions at 10 hot files (1 before), that contention's
own; oracle clean.
