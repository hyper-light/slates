#!/usr/bin/env python3
"""The peek-until-whole-record pattern slates' NFS server uses (A-113), with no NFS: a server that peeks a record's
marker, sets SO_RCVLOWAT to the whole record and peeks it, consuming it only after replying, against a client sending
256.5 KiB records with INFLIGHT outstanding. Run in a Linux container, each run bounded:
  timeout -s KILL 15 python3 peek_lowat_repro.py N cold INFLIGHT [CAP]
CAP, when given, caps the low-water mark at SO_RCVBUF / CAP. On Linux 6.12 (linuxkit, 2026-10-08) most runs hang: 8 of
10 at INFLIGHT 4, 9 of 10 at 32, and 10 of 10 at 32 with the low-water mark capped at half the buffer
(`docs/bugs/2026-10-08-the-first-large-nfs-write-on-linux-stalls-ten-seconds.md`).
"""
import socket, struct, threading, time, sys
RECORD = int(__import__("os").environ.get("RECORD", 262144 + 512))  # one WRITE record
PRIME = int(__import__("os").environ.get("PRIME", 0))  # raise the low-water mark to PRIME records once, at accept
CAP = int(sys.argv[4]) if len(sys.argv) > 4 else 0
INFLIGHT = int(sys.argv[3]) if len(sys.argv) > 3 else 4
N = int(sys.argv[1]) if len(sys.argv) > 1 else 64
SMALL_FIRST = len(sys.argv) > 2 and sys.argv[2] == "small-first"
srv = socket.socket(); srv.bind(("127.0.0.1", 0)); srv.listen(1); port = srv.getsockname()[1]
def server():
    c, _ = srv.accept()
    if PRIME:
        # Grow the receive buffer once: Linux raises an unlocked socket's buffer to fit a raised low-water mark
        # (`tcp_set_rcvlowat`), up to tcp_rmem[2]; the mark is then set per record as before.
        c.setsockopt(socket.SOL_SOCKET, socket.SO_RCVLOWAT, PRIME * RECORD)
        c.setsockopt(socket.SOL_SOCKET, socket.SO_RCVLOWAT, 1)
    got = 0
    while got < N + (8 if SMALL_FIRST else 0):
        c.setsockopt(socket.SOL_SOCKET, socket.SO_RCVLOWAT, 4)        # the marker alone, not the last record's size
        head = c.recv(4, socket.MSG_PEEK)
        if not head: return
        if len(head) < 4: time.sleep(0); continue
        need = 4 + (struct.unpack(">I", head)[0] & 0x7fffffff)
        lowat = need
        if CAP:
            lowat = min(need, c.getsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF) // CAP)
        c.setsockopt(socket.SOL_SOCKET, socket.SO_RCVLOWAT, lowat)  # wake when the record is whole, or the cap
        data = c.recv(need, socket.MSG_PEEK)                        # peek, blocking until lowat
        if len(data) < need: continue
        c.recv(need)                                                 # consume after the "reply"
        c.sendall(b"ok")
        got += 1
threading.Thread(target=server, daemon=True).start()
cl = socket.create_connection(("127.0.0.1", port))
def reply():
    got = b""
    while len(got) < 2:                                              # exactly one two-byte reply, however it arrives
        chunk = cl.recv(2 - len(got))
        if not chunk: raise SystemExit("server closed")
        got += chunk
def send(size):
    cl.sendall(struct.pack(">I", 0x80000000 | (size - 4)) + b"\0" * (size - 4))
if SMALL_FIRST:
    for _ in range(8): send(256); reply()
t = time.time()
pending = 0
for i in range(N):
    send(RECORD); pending += 1
    while pending > INFLIGHT: reply(); pending -= 1      # a few in flight, as a client's slots allow
for _ in range(pending): reply()
el = time.time() - t
print(f"{'small-first' if SMALL_FIRST else 'cold'}: {N} records of {RECORD} B in {el:.2f} s ({N*RECORD/el/1e6:.0f} MB/s)")
