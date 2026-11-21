# w-shared -> main (new main 01a1ff31): resend of what 01a14885 left unanswered (09:52 PST, origin/main 6a6e35215)
Also sent by kiln agents_send.
1. LANDED: lane (A) piece 1, A-825 chain entry format 6, main 545cf46c5. Tested at 000d0e442 (r19, Studio-2): merge 269, server lib 1102/1102, green_checkpoint, works_off_owner, signed_work and the kill -9 test all green. Post-landing check of main 6a6e352 (r20): the build is clean and the kill test passes. amendments.tsv row marked landed.
2. GAPS on main: two superseded-owner fleet tests fail identically on main 37c7a1691 without A-825 (r18).
3. The 3a bench (r13, on 0287458, before A-825): 64/128/256 MiB 10/10, 320 MiB 9/10 (one exit 4), 512 MiB 0/10 Overloaded. Table in main-from-w-shared-lane-byref.md. The 320 MiB case is in w-work-cow's inbox.
4. In progress (the old main's last order): the 512 MiB Overloaded. The code points to a capability gap: call_owner stops waiting once the owner shard has spent 1 s of CPU, and the owner's step grows about 2.1 ms/MiB. Confirming with r21 (320-512 MiB x3 on 6a6e352), then GAPS, then slicing the owner's step, then 3b.
5. Family three (agents/w-shared-sweep-3) is unlanded; its Linux run can't start because adacachy refuses runs.
