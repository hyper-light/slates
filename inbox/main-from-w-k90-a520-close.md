# w-k90-a520 -> main: close notes

## C1 CLOSED: the session-end fix (w-k90-containers' r49: volume gone while its container was listed)
- Commits on origin/main: kiln 2f589baa6 (the release is one claim the run's end waits on; `volume rm` retried, the
  export detached whatever it said; red first: w9_a_run_reads_ended_only_once_its_container_is_released red
  ["ended","released"], w9_an_engine_volume_still_in_use_... red) and kiln cc80a38f8 (a container session waits up to
  60 s for its run's exit, after its release, before it reads ended: the stop path waited 5 s or not at all).
- Tests: unit 16/16 (macOS arm64, Studio-4, local; incl.
  w9_a_container_session_waits_out_its_containers_release_before_it_reads_ended); e2e checks 18/18.
- By use: w-k90-containers' r56 on kiln cc80a38f8, Adas-MacBook-Pro-2, Docker Desktop 29.3.1: the Ask session's step 4
  PROVEN "ended, its container and engine volume gone and its mount out of the host's table".

## The containers part (Ada's corrected goal: a slates volume attaches to an OCI container), proven by use
- adacachy, rootful Docker Engine 29.9.0, no machine change: run r3 on kiln 209e19dc6 (same code as main f5056c0bb):
  slates_e2e container:: 2 passed, 0 failed, 240.9 s. An agent's container ran on the engine's NFS volume of the
  volume's slates export; edits seen through the host's own mount; change request exact; the person's landing wrote
  exactly its changes; drift kept the person's bytes; container, engine volume and mount gone at the end.
- Mac (Docker Desktop 29.3.1): w-k90-containers' r4 on 02f311ec3 (209e19dc6 + its Windows port), 2/2.
- A/B on adacachy (r5, release kiln, 5 rounds interleaved): the export made what the container's own tmpfs made, byte for
  byte, in git, cargo --release, npm and fs, every run's seconds in kiln docs/agentic.md W9 (2dc0b1a95); the bind was
  refused 20/20 without allow_other. Commits on kiln main: a82ce7ede (export route, bind removed), 2f589baa6,
  cc80a38f8, f5056c0bb (export_engine, drive and A/B fixes), 2dc0b1a95 (docs); Slates ae8319811 (A-769).
- Out of scope now, owed to the kiln engine rework: Mac bind-vs-export timings (r9 stopped before it ran).
