# A-636's red checks: each mutation removes one rule; the tests that name it must fail.
import os, subprocess, sys
FILE = "crates/db/src/ledger/logs.rs"
MUTATIONS = [
  ("keep_whole_off", "    let dropped = keep_whole(&mut owners, &cohort.records);\n",
   "    let dropped: Vec<[u8; 32]> = Vec::new();\n"),
  ("precheck_off", "    for (name, log) in &cohort.logs {\n      log\n        .can_take_over(reachable)\n        .map_err(|error| LogsTakeoverRefused { log: *name, error })?;\n    }\n", ""),
  ("split_off", "        .is_some_and(|spans| spans_split_by(spans, at))\n",
   "        .is_some_and(|spans| spans_split_by(spans, at) && spans.is_empty())\n"),
  ("applied_bound_off", "      if *asked > applied {\n", "      if *asked > applied && applied > *asked {\n"),
  ("moved_off", "      if span.from != at {\n", "      if span.from != at && span.from == at {\n"),
  ("commit_tracking_off", "      if last.is_some_and(|commit| commit.committed) {\n",
   "      if last.is_some_and(|commit| commit.committed && commit.acked.is_empty()) {\n"),
  ("announce_apply_off", "      cohort.apply(*host, held, &cut);\n", "      let _ = (host, held);\n"),
  ("cut_not_whole", "    whole_cut(limits, &identity_at, &cohort.records)\n",
   "    let _ = (&identity_at, &cohort.records);\n    limits\n"),
]
def main():
  if len(sys.argv) > 1 and sys.argv[1] == "check":
    text = open(FILE).read()
    for name, old, _ in MUTATIONS:
      print(name, text.count(old))
    return
  pristine = open(FILE).read()
  for name, old, new in MUTATIONS:
    if pristine.count(old) != 1:
      print(name, "TARGET_NOT_UNIQUE", pristine.count(old)); continue
    open(FILE, "w").write(pristine.replace(old, new))
    run = subprocess.run(["cargo", "test", "-p", "slates-db", "--lib", "ledger::logs"],
                         capture_output=True, text=True)
    out = run.stdout + run.stderr
    failed = [line.split()[1] for line in out.splitlines() if line.startswith("test ") and line.endswith("FAILED")]
    compiled = "error[" not in out and "error:" not in out.split("test result")[0]
    verdict = "RED" if run.returncode != 0 and failed else ("NO_BUILD" if not compiled else "GREEN")
    print(name, verdict, "failed:", ",".join(sorted(set(failed))))
    open(FILE, "w").write(pristine)
main()
