"""Remove agents' finished clones and worktrees under ~/slates-work, keeping anything that holds work.

A directory is removed only when ALL hold:
- it is a git repository or worktree;
- `git status --porcelain` prints nothing (no uncommitted or untracked files);
- it has no stash entries;
- every local branch head and HEAD is in origin/main's history (landed; pushed alone is not enough);
- nothing under it (outside .git) changed in the last RECENT_MINUTES minutes;
- for a clone: none of its other worktrees is kept.
Worktrees go first (`git worktree remove`), then clones (rm -rf). Run with --execute to act; default is a dry run.
"""
import os, shutil, subprocess, sys, time

ROOT = os.path.expanduser("~/slates-work")
RECENT_MINUTES = 180  # Shape: an agent's piece runs at most a few hours; anything touched since may be live.
EXECUTE = "--execute" in sys.argv


def git(path, *args):
    r = subprocess.run(["git", "-C", path, *args], capture_output=True, text=True)
    return r.returncode, r.stdout.strip()


def recent(path):
    cutoff = time.time() - RECENT_MINUTES * 60
    for dirpath, dirnames, filenames in os.walk(path):
        if ".git" in dirnames:
            dirnames.remove(".git")
        for name in filenames:
            try:
                if os.lstat(os.path.join(dirpath, name)).st_mtime > cutoff:
                    return True
            except OSError:
                pass
    return False


SUPERVISOR = os.path.expanduser("~/slates-work/supervisor")


def landed(sha):
    """The commit is in main's history (the supervisor clone's freshly fetched origin/main)."""
    r = subprocess.run(["git", "-C", SUPERVISOR, "merge-base", "--is-ancestor", sha, "origin/main"], capture_output=True)
    return r.returncode == 0


def unpushed(path):
    # Landed, not merely pushed: every local branch head and HEAD must be in main's history. A pushed but unlanded
    # branch is a live agent's work in progress (2026-11-19: the pushed-is-enough test removed parked agents' worktrees
    # during the Studio outage; nothing was lost, but they had to be made again).
    code, heads = git(path, "for-each-ref", "--format=%(objectname)", "refs/heads")
    code2, head = git(path, "rev-parse", "HEAD")
    shas = [h for h in heads.splitlines() if h] + ([head] if code2 == 0 and head else [])
    return not shas or not all(landed(sha) for sha in set(shas))


def holds_work(path):
    code, status = git(path, "status", "--porcelain")
    if code != 0 or status:
        return "uncommitted"
    code, stash = git(path, "stash", "list")
    if code != 0 or stash:
        return "stash"
    if unpushed(path):
        return "unpushed"
    if recent(path):
        return "recent"
    return None


entries = sorted(os.listdir(ROOT))
worktrees, clones, kept = [], [], []
for name in entries:
    path = os.path.join(ROOT, name)
    if not os.path.isdir(path):
        continue
    code, common = git(path, "rev-parse", "--git-common-dir")
    if code != 0:
        kept.append((name, "not git"))
        continue
    common = os.path.normpath(os.path.join(path, common))
    is_clone = common == os.path.join(path, ".git")
    reason = holds_work(path)
    if reason:
        kept.append((name, reason))
    elif is_clone:
        clones.append(path)
    else:
        worktrees.append((path, common))

kept_paths = {os.path.join(ROOT, n) for n, _ in kept}
# A clone with any kept worktree stays.
safe_clones = []
for path in clones:
    code, out = git(path, "worktree", "list", "--porcelain")
    attached = [l.split(" ", 1)[1] for l in out.splitlines() if l.startswith("worktree ")]
    if any(a in kept_paths for a in attached if os.path.normpath(a) != path):
        kept.append((os.path.basename(path), "has a kept worktree"))
    else:
        safe_clones.append(path)

print(f"remove worktrees: {len(worktrees)}; remove clones: {len(safe_clones)}; keep: {len(kept)}")
reasons = {}
for _, r in kept:
    reasons[r] = reasons.get(r, 0) + 1
print("kept by reason:", reasons)
if not EXECUTE:
    sys.exit(0)
removed = 0
for path, common in worktrees:
    repo = os.path.dirname(common)
    code, _ = git(repo, "worktree", "remove", path)
    if code == 0:
        removed += 1
for path in safe_clones:
    shutil.rmtree(path, ignore_errors=True)
    removed += 1
print("removed:", removed)
