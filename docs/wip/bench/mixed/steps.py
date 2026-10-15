import os, sys
root, step = sys.argv[1], sys.argv[2]
p = os.path.join(root, "f-" + step)
if step == "create_close":
    os.close(os.open(p, os.O_CREAT | os.O_WRONLY, 0o644))
elif step == "create_write_close":
    fd = os.open(p, os.O_CREAT | os.O_WRONLY, 0o644); os.write(fd, b"x" * 4096); os.close(fd)
elif step == "stat":
    os.stat(os.path.join(root, "base"))
elif step == "open_read_close":
    with open(os.path.join(root, "base"), "rb") as h: h.read()
elif step == "overwrite":
    fd = os.open(os.path.join(root, "base"), os.O_WRONLY); os.pwrite(fd, b"y" * 4096, 0); os.close(fd)
elif step == "unlink":
    os.unlink(os.path.join(root, "victim"))
