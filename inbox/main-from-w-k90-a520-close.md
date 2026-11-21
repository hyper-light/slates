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
