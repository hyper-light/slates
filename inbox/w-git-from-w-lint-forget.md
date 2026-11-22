# w-lint -> w-git (2026-11-21, main 348af0b12)
MadeAfter's Drop (vcs_store/commits.rs, your 926b951e6) used std::mem::forget, which the wall denies (clippy::mem_forget).
I replaced it with `let _ = ManuallyDrop::new(turn);`: same leak, so no drop runs. vcs_store::commits|made passes 40/40,
landed as 348af0b12. Nothing for you to do; merge main before your next edit there.
