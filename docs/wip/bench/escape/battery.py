#!/usr/bin/env python3
"""A generated escape battery (condition 4): seeded random hostile operations through a slates FUSE mount, as the
volume's owner (an agent) and another user (a privileged tool), with the containment invariants checked after every
step. Usage: battery.py MOUNT CANARY_DIR SEED STEPS. Exits non-zero naming the first violated invariant."""
import os, random, sys, errno, subprocess, pwd

mount, canary_dir, seed, steps = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
rng = random.Random(seed)
owner = pwd.getpwnam("tester"); other = pwd.getpwnam("other")
canary = os.path.join(canary_dir, "canary")
canary_bytes = open(canary, "rb").read()
outside_targets = [canary, canary_dir, "/etc/passwd", "/etc", "/proc/self/root" + canary, "/proc/1/root/etc/hostname",
                   "/", "../" * 8 + canary.lstrip("/"), "../" * 3 + "etc/shadow", "/dev/sda", "/dev/mem"]
violations = []
counts = {}

def note(kind):
    counts[kind] = counts.get(kind, 0) + 1

def as_user(user, code):
    """Runs `code` (a python snippet reading PATH/ARG from argv) as `user`; returns (exit status, stdout)."""
    r = subprocess.run(["setpriv", f"--reuid={user.pw_uid}", f"--regid={user.pw_gid}", "--clear-groups",
                        sys.executable, "-c", code], capture_output=True, text=True, timeout=30)
    return r.returncode, r.stdout.strip()

def name():
    kind = rng.randrange(6)
    if kind == 0: return "n" + str(rng.randrange(100000))
    if kind == 1: return "x" * rng.choice([1, 254, 255])
    if kind == 2: return "sp ace" + str(rng.randrange(100))
    if kind == 3: return "nl\n" + str(rng.randrange(100))
    if kind == 4: return "dot.." + str(rng.randrange(100))
    return "u\u00e9" + str(rng.randrange(100))

def some_dir():
    dirs = [mount]
    for root, ds, _ in os.walk(mount):
        for d in ds:
            p = os.path.join(root, d)
            if not os.path.islink(p): dirs.append(p)
        if len(dirs) > 50: break
    return rng.choice(dirs)

READ_THROUGH = "import sys\ntry:\n  print(open(sys.argv[1],'rb').read()[:64].hex())\nexcept OSError as e:\n  print('ERR', e.errno)\n"
WRITE_THROUGH = "import sys\ntry:\n  open(sys.argv[1],'ab').write(b'ESCAPED')\n  print('WROTE')\nexcept OSError as e:\n  print('ERR', e.errno)\n"

def run_as(user, code, *args):
    r = subprocess.run(["setpriv", f"--reuid={user.pw_uid}", f"--regid={user.pw_gid}", "--clear-groups",
                        sys.executable, "-c", code, *args], capture_output=True, text=True, timeout=30)
    return r.stdout.strip()

for step in range(steps):
    op = rng.randrange(8)
    d = some_dir()
    try:
        if op == 0:  # an agent plants a link out of the volume; the other user reads and writes through it
            target = rng.choice(outside_targets)
            link = os.path.join(d, name())
            r = run_as(owner, "import os,sys\ntry:\n  os.symlink(sys.argv[1], sys.argv[2]); print('OK')\nexcept OSError as e: print('ERR', e.errno)", target, link)
            if r == "OK":
                note("link_out")
                # The positive control: the link's owner resolves it (A-107 lets the owner follow its own link), so
                # the other user's refusal below is the rule working, not links being dead.
                if target == canary and run_as(owner, READ_THROUGH, link) == canary_bytes[:64].hex():
                    note("owner_resolved_canary")
                read = run_as(other, READ_THROUGH, link)
                if read.startswith("ERR"):
                    note("other_refused")
                if read and not read.startswith("ERR") and canary_bytes[:64].hex() == read:
                    violations.append(f"step {step}: the other user read the canary through {link!r} -> {target}")
                wrote = run_as(other, WRITE_THROUGH, link)
                if wrote == "WROTE" and os.path.realpath(link).startswith(mount) is False:
                    violations.append(f"step {step}: the other user wrote outside through {link!r} -> {target}")
        elif op == 1:  # hard links across the boundary, both ways
            src_out = rng.choice([canary, "/etc/hostname"])
            r = run_as(owner, "import os,sys\ntry:\n  os.link(sys.argv[1], sys.argv[2]); print('LINKED')\nexcept OSError as e: print('ERR', e.errno)", src_out, os.path.join(d, name()))
            note("hardlink_in")
            if r == "LINKED": violations.append(f"step {step}: a hard link into the volume from {src_out} succeeded")
        elif op == 2:  # a device node, a fifo
            p = os.path.join(d, name())
            r = run_as(owner, "import os,sys,stat\ntry:\n  os.mknod(sys.argv[1], 0o666|stat.S_IFCHR, os.makedev(1,1)); print('MADE')\nexcept OSError as e: print('ERR', e.errno)", p)
            note("mknod")
            if r == "MADE": violations.append(f"step {step}: a character device was made at {p!r}")
        elif op == 3:  # a setuid copy of id: it must not raise the runner's euid
            p = os.path.join(d, name())
            run_as(owner, "import shutil,os,sys\ntry:\n  shutil.copy('/usr/bin/id', sys.argv[1]); os.chmod(sys.argv[1], 0o4755)\nexcept OSError: pass", p)
            if os.path.exists(p):
                note("setuid")
                r = subprocess.run(["setpriv", f"--reuid={other.pw_uid}", f"--regid={other.pw_gid}", "--clear-groups", p, "-u"], capture_output=True, text=True)
                if r.stdout.strip() not in ("", str(other.pw_uid)):
                    violations.append(f"step {step}: a setuid file ran as uid {r.stdout.strip()}")
        elif op == 4:  # rename a planted link into nested directories, then traverse it as the other user
            links = [os.path.join(r_, f) for r_, _, fs in os.walk(mount) for f in fs if os.path.islink(os.path.join(r_, f))]
            if links:
                src = rng.choice(links); nested = os.path.join(some_dir(), name())
                run_as(owner, "import os,sys\ntry: os.rename(sys.argv[1], sys.argv[2])\nexcept OSError: pass", src, nested)
                note("rename_link")
                read = run_as(other, READ_THROUGH, nested)
                if read and not read.startswith("ERR") and read == canary_bytes[:64].hex():
                    violations.append(f"step {step}: the canary read through a renamed link {nested!r}")
        elif op == 5:  # ordinary churn: files and directories
            p = os.path.join(d, name())
            run_as(owner, "import os,sys,random\ntry:\n  if random.random()<0.3: os.mkdir(sys.argv[1])\n  else: open(sys.argv[1],'wb').write(os.urandom(random.randrange(1,9000)))\nexcept OSError: pass", p)
            note("churn")
        elif op == 6:  # .. climbing in a path through the mount
            p = os.path.join(d, *([".."] * rng.randrange(1, 10)), "canary")
            read = run_as(other, READ_THROUGH, p)
            note("dotdot")
            real = os.path.realpath(p)
            if read and not read.startswith("ERR") and read == canary_bytes[:64].hex() and not real.startswith(canary_dir):
                violations.append(f"step {step}: .. reached the canary at {p!r}")
        else:  # remove something
            entries = [os.path.join(r_, f) for r_, _, fs in os.walk(mount) for f in fs]
            if entries:
                run_as(owner, "import os,sys\ntry: os.unlink(sys.argv[1])\nexcept OSError: pass", rng.choice(entries))
            note("remove")
    except subprocess.TimeoutExpired:
        violations.append(f"step {step}: op {op} hung past 30 s")
    if open(canary, "rb").read() != canary_bytes:
        violations.append(f"step {step}: the canary outside the volume changed")
        break

print("ops:", " ".join(f"{k}={v}" for k, v in sorted(counts.items())))
if violations:
    print("VIOLATIONS:"); [print(" ", v) for v in violations[:20]]
    sys.exit(1)
print(f"seed {seed}: {steps} steps, no invariant violated")
