# w-k90-pin-2 close note (2026-11-21 ~04:35 PST)

## C1 goal 90a: every platform's lane green -- CLOSED
vendor/slates at 8ce3d61523e1 in every run below. GitHub ci.yml `slates` jobs (windows-slates for x86-64 Windows):
- macOS arm64: run 44851246181 job 136703754428 (kiln 4f5e58c1b)
- Linux x86-64: run 44851246181 job 136703754443 (kiln 4f5e58c1b)
- Linux arm64: run 44887857088 job 136804151244 (kiln a0d114abb)
- Windows x86-64: run 44887857088 job 136804151233 (kiln a0d114abb; w-k90-win's job, its f16212799 in)
- Windows arm64: run 44887857088 job 136804151286 (kiln a0d114abb)
Commits on origin/main: c0f576eb3, 5264caaa3, e763e394f, eb7194f00, f500759d2, 482bd01f8, 60c0b27c5.
Test: tests/vendored.rs 11/11 local with an LF and a CRLF Cargo.lock; no slates CLI call (git grep a0d114abb).
Goal 01a1facaf183 marked met.

## R2 the runner link closing (cec37f60, b24c5c95): IN PROGRESS
- 2026-11-21 ~13:30 PST: R2's Linux and macOS pieces LANDED on kiln main ae1c43321 (fix 62912688c, base 8491937cf):
  IP_PKTINFO in kiln-rt (kiln's own hyper-rt) + vendored hyper-raft change 0006 (handle_datagram_to; driver sends
  from src_ip). Red/green on Linux without privilege: kiln-rt udp_local_address
  reached_at_a_second_address_it_answers_from_that_address (0.0.0.0 reached at 127.0.0.2: the kernel answered from
  127.0.0.1; the fix answers from 127.0.0.2). macOS green (sendmsg + IP_PKTINFO answer arrives). Windows: written up in
  progress/w-k90-pin-2.md (WSARecvMsg/WSASendMsg with IN_PKTINFO; waits for a Windows-hosted server).
  Not landed: kiln's end-to-end tunnel test, never run (no runner room); kept for a follow-up.
  R2 closes on measurement once a server carrying ae1c43321 (or Ada's .77 change) is live.
- Studio-4: Docker is wedged (containers stuck "Created" 20-30 min, other agents' alpine cleanup runs too), and free
  disk is ~7 GB (not mine: my target is deleted). For a person.
