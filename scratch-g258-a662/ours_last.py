import sys
path = sys.argv[1]
lines = open(path, encoding="utf-8").read().split("\n")
out, ours, theirs, state = [], [], [], 0
for line in lines:
    if state == 0 and line.startswith("<<<<<<< "):
        state = 1
    elif state == 1 and line == "=======":
        state = 2
    elif state == 2 and line.startswith(">>>>>>> "):
        while theirs and theirs[-1] == "":
            theirs.pop()
        while ours and ours[-1] == "":
            ours.pop()
        out.extend(theirs)
        out.append("")
        out.extend(ours)
        ours, theirs, state = [], [], 0
    elif state == 1:
        ours.append(line)
    elif state == 2:
        theirs.append(line)
    else:
        out.append(line)
if state != 0:
    sys.exit("unterminated conflict")
open(path, "w", encoding="utf-8").write("\n".join(out))
