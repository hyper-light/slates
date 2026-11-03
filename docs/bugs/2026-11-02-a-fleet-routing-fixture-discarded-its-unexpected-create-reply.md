# A fleet routing fixture discarded its unexpected Create reply

Observed 2026-11-02 while requiring green main CI for Step 9.4. The fleet history serves §4.8 Lookup; its setup
failure is independent of the Windows takeover and QEMU histories, which pass on the same commit.

## Description and evidence

Main CI [41538865390](https://github.com/hyper-light/slates/actions/runs/41538865390), commit `8a3fdc63`, Linux job
[125597589278](https://github.com/hyper-light/slates/actions/runs/41538865390/job/125597589278), fails one of 86 active
fleet cases (85 pass, three child roles ignored):
`a_location_round_asks_a_peer_whose_session_was_out_once_it_returns`. Its trace reaches a Create reply, then panics
at `choose_copyset_routing_case`, line 5609, with only `create routing candidate`. The target history has not reached
the held-session location round. The raw unexpected reply is absent, so neither a refusal kind nor a product cause
can be inferred from this log.

The same fleet source passes the Linux main lane on parent `663e4df8` (run 41535967224, job 125587750245). Commit
`8a3fdc63` changes the live VM fixture and documentation, not the fleet source. This distinguishes the failed setup
from the already verified VM and Windows changes; it does not diagnose the fleet failure.

## Diagnostic cause and exact edit

The helper matches `local.call(Create)` directly in a let-else and discards the unexpected `ReplyBody` before its
literal panic. It now binds that reply first and prints both the candidate name and its complete typed verdict on
the existing failing branch. The selected-id logic, expected Created reply, assertions and finite candidate budget
are unchanged. A future failure retains the information needed to decide whether setup or provisioning is wrong.

No fleet behavior fix is claimed: the original unexpected Create reply remains unattributed.

## Verification

On adacachy, 2026-11-02, the existing history with the improved diagnostic passes once in 5.25 s:

```sh
cargo +1.98.1 test --locked -p slates-server --test fleet \
  a_location_round_asks_a_peer_whose_session_was_out_once_it_returns -- --exact --nocapture --test-threads=1
```

The owned systemd unit has CPU/memory caps derived from one quarter of available resources, its own Cargo target,
and a finite runtime; build plus run takes 19.266 s. Log: `~/.cache/slates-s9-fleet-setup-diagnostic.log`. One passing
isolated history establishes neither the original verdict nor its cause. The existing complete main CI lane is
still required; its test list is unchanged.

The workspace gate chain passes in 10.005 s in the same capped unit: `cargo +1.98.1 fmt --check &&
cargo +1.98.1 clippy --locked --workspace --all-targets -- -D warnings && cargo +1.98.1 xtask check &&
RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" cargo +1.98.1 doc --locked --workspace --no-deps
--document-private-items`. Log: `~/.cache/slates-s9-fleet-diagnostic-gates.log`.
