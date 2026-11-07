# A held origin refused every checkout

**Found** 2026-11-05 by rebasing agents/fs-server-3 onto main b502cf96: `cargo +1.98.1 test -p slates-server --test
checkout` failed 3 of 3 on main itself (it passed on a7804447), two with `Refused(ConsistentBaseUnavailable)` at the
checkout and one with that refusal where `Unsupported` was expected; `crates/cli/tests/fs_server.rs` failed the same
way at its checkout.

**Root cause.** A-217 (`6ebc54ea`, loadtest's, landed in b502cf96) records a based green's origin as a reference to its
base snapshot with the origin's BLAKE3 identity (`Op::GreenOriginHeld`, `Partition::green_origin_ref`) and no longer its
bytes (`Partition::green_origin`). The checkout (A-201, `crates/server/src/checkout.rs`) read the bytes, found none, and
took the green for a scratch one; once given bytes it compared encodings. Each landing's tests passed on its own branch;
the two met only on main.

**Impact.** On main b502cf96, no based green could be checked out, so no merged work reached a disk (A-201's whole
purpose), on one host or across a fleet. Nothing was written wrongly: the refusal came before any step.

**Fix.** The checkout compares identities: the named snapshot's origin's (`merge_service::origin_identity`) against the
reference's, or against the BLAKE3 of the bytes a taken-over green still records (A-217's owed fleet path); a green
with neither is scratch and still refused. After: the checkout suite 3 of 3 and the two-node CLI test 1 of 1 on
b502cf96 (adacachy, 2026-11-05). The failing tests were the existing ones, red on main.

**Siblings checked.** `green_origin` is read elsewhere by `inputs_of` and recovery, which A-217 changed itself; nothing
else of A-201 reads the origin.
