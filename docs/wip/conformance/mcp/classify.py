import json, sys, glob, os, collections
counts = collections.Counter()
other = []
for d in sorted(glob.glob(os.path.join(sys.argv[1], "results", "server-*"))):
    name = os.path.basename(d).split("-2026")[0].replace("server-", "", 1)
    for c in json.load(open(os.path.join(d, "checks.json"))):
        if c["status"] in ("SUCCESS", "SKIPPED", "INFO"):
            continue
        m = c.get("errorMessage", "") or ""
        if any(k in m for k in ("test_", "test://", "Not testable", "json_schema_2020_12_tool")):
            counts["fixture"] += 1
        elif name.startswith("tasks-"):
            counts["tasks-extension"] += 1
            other.append(("tasks", name, c["id"], m[:110]))
        else:
            counts["other"] += 1
            other.append(("other", name, c["id"], m[:110]))
print(dict(counts))
for row in other:
    print("  ", *row)
