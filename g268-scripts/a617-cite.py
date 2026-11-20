"""Corrects edit.rs's and its tests' line citations to Subversion's 1.14.x at d434dc14 (each old text exactly once)."""
import pathlib, sys
root = pathlib.Path(sys.argv[1])
FIXES = {
  "crates/svn/src/commit/edit.rs": [
    ("(`serve.c` `commit` 1471-1550)", "(`serve.c` `commit` 1470-1583, its parameters at 1495-1510)"),
    ("`svn_ra_svn_drive_editor2` 1073-1211 and its handlers 532-933", "`svn_ra_svn_drive_editor2` 1073-1212 and its handlers 532-935"),
    ("`vparse_tuple` (1608-1715)", "`vparse_tuple` (1608-1718)"),
    ("the edit aborted (1105-1110)", "the edit aborted (1127-1132, written at 1149-1174)"),
    ("casts an `r` number to it (1652)", "casts an `r` number to it (1643-1644)"),
    ("(`editorp.c` 598-605)", "(`editorp.c` 601-609)"),
    ("(`editorp.c` 922-933)", "(`editorp.c` 922-935)"),
    ("(`editorp.c` 1110-1121)", "(`editorp.c` 1115-1126)"),
    ("(`serve.c` 1520-1525)", "(`serve.c` 1526-1534)"),
    ("(`serve.c` 1395-1430)", "(`serve.c` 1360-1409)"),
    ("`\"(?r)s\"` (`editorp.c` 553)", "`\"(?r)s\"` (`editorp.c` 555)"),
    ("`\"c(?r)s\"` (572)", "`\"c(?r)s\"` (574)"),
    ("`\"css(?cr)\"` (596)", "`\"css(?cr)\"` (595)"),
    ("`\"css(?r)\"` (631)", "`\"css(?r)\"` (630)"),
    ("`\"css(?r)\"` (745)", "`\"css(?r)\"` (746)"),
    ("`\"s(?c)\"` (773)", "`\"s(?c)\"` (774)"),
    ("always does (`serve.c` 1506)", "always does (`serve.c` 1501)"),
  ],
  "crates/svn/src/commit/edit/tests.rs": [
    ("(`vparse_tuple` 1608-1715:", "(`vparse_tuple` 1608-1718:"),
    ("`vparse_tuple` (1608-1715), transcribed", "`vparse_tuple` (1608-1718), transcribed"),
    ("`editorp.c` 1103-1108", "`editorp.c` 1127-1132"),
    ("`serve.c` `commit` 1497-1510 and `add_lock_tokens` 1395-1430", "`serve.c` `commit` 1495-1510 and `add_lock_tokens` 1360-1409"),
    ("`editorp.c` 532-933", "`editorp.c` 532-935"),
    ("`editorp.c` 762-827", "`editorp.c` 762-826"),
  ],
}
for name, fixes in FIXES.items():
  path = root / name
  text = path.read_text()
  for old, new in fixes:
    count = text.count(old)
    if count != 1:
      raise SystemExit(f"{name}: {old!r} occurs {count} times")
    text = text.replace(old, new)
  path.write_text(text)
print("citations corrected")
