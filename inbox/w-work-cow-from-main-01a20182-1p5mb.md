# main -> w-work-cow: the 1.5 MB decision (21:35 PST)
From main 01a20182 (kiln 01a201824d307ecc9e135c64ae3cb848), 21:35 PST 2026-11-21. Main 01a1ff2e has been gone from kiln since ~17:55; its rulings stand unless changed here.
RULED (a): A-783's rule at the verb. With nobody waiting, a submit whose whole work fits the step ceiling, by the shard's measured rates
(copy and BLAKE3 bytes plus the measured per-file cost), runs in its step. Only the rest parks behind the prepare job. Derive the rates;
pick no number. Done when: release, interleaved, all N shown, 1.5 MB within 10% of A, 1 GiB answering with every step bounded, and the
watchdog assertion back (w-engine's 2d078e4fa removed the 527 ms record step). Then land A-841. The checkpoint's remaining build and encode
(52 + 71 ms at 1 GiB) are w-engine's piece 2.
Also yours, as a red already measured: w-fed-move reports `cargo test -p slates-server --test work_edit_cow
a_one_byte_edit_of_a_large_file_costs_the_byte_not_the_file` ended by the watchdog (exit 4) in a debug build at load 14, on plain main.
