#!/usr/bin/env python3
"""A minimal loopback CGI host for `git http-backend` (slates-git's smart HTTP golden vectors, A-595).

It serves HTTP/1.1 on 127.0.0.1, runs `git http-backend` once per request as its CGI program (RFC 3875), and records
each exchange as NN-<method>-<last path segment>.<part>, holding inflated bytes alone (main's rule with A-607): never a
gzip body as git compressed it, and never a pack.

  .request-head.bin     the request's head as git sent it, through its empty line
  .request-body.bin     a body of plain pkt-lines, as sent (Content-Length framing, no coding)
  .request-inflated.bin a gzip body, inflated by Python's gzip module (an inflater apart from slates')
  .request-chunked.bin  a chunked body of plain pkt-lines, its framing and data as sent
  .request-data.sha256  the SHA-256 of that chunked body's data, as this host removed the framing
  .reply-head.bin       http-backend's CGI head, through its empty line
  .reply-body.bin       an advertisement's body (GET info/refs), as http-backend wrote it

A push's body is not held (its pack is compressed), nor is any POST's reply body (a fetch's is a pack).
A chunked body is handed to the CGI program whole, with its length, as a web server hands one. Standard library only.

Usage: cgi-host.py PORTFILE OUTDIR PROJECT_ROOT
"""
import gzip
import hashlib
import os
import socket
import subprocess
import sys
import threading

LOCK = threading.Lock()
COUNT = [0]


def fill(conn, buf, need):
    """Reads until `need(buf)` holds; returns the buffer, or None if the peer closed first."""
    while not need(buf):
        data = conn.recv(65536)
        if not data:
            return None
        buf += data
    return buf


def chunked(conn, buf):
    """The raw chunked body, its data, and the bytes after it."""
    raw, body = b"", b""
    while True:
        buf = fill(conn, buf, lambda b: b"\r\n" in b)
        line, buf = buf.split(b"\r\n", 1)
        raw += line + b"\r\n"
        size = int(line.split(b";")[0], 16)
        if size == 0:
            while True:
                buf = fill(conn, buf, lambda b: b"\r\n" in b)
                trailer, buf = buf.split(b"\r\n", 1)
                raw += trailer + b"\r\n"
                if trailer == b"":
                    return raw, body, buf
        buf = fill(conn, buf, lambda b: len(b) >= size + 2)
        raw += buf[: size + 2]
        body += buf[:size]
        buf = buf[size + 2 :]


def save(out, name, data):
    with open(os.path.join(out, name), "wb") as f:
        f.write(data)


def record_request(out, name, path, fields, raw, body):
    """Holds a request's body as the module's docstring says."""
    if not body or path.endswith("/git-receive-pack"):
        return
    if fields.get("content-encoding", "").lower() in ("gzip", "x-gzip"):
        save(out, name + ".request-inflated.bin", gzip.decompress(body))
    elif fields.get("transfer-encoding", "").lower() == "chunked":
        save(out, name + ".request-chunked.bin", raw)
        save(out, name + ".request-data.sha256", (hashlib.sha256(body).hexdigest() + "\n").encode())
    else:
        save(out, name + ".request-body.bin", body)


def serve(conn, out, root):
    buf = b""
    while True:
        buf = fill(conn, buf, lambda b: b"\r\n\r\n" in b)
        if buf is None:
            return
        head, buf = buf.split(b"\r\n\r\n", 1)
        lines = head.decode("latin-1").split("\r\n")
        method, target, _ = lines[0].split(" ")
        fields = {}
        for line in lines[1:]:
            name, value = line.split(":", 1)
            fields[name.strip().lower()] = value.strip()
        if fields.get("expect", "").lower() == "100-continue":
            conn.sendall(b"HTTP/1.1 100 Continue\r\n\r\n")
        if fields.get("transfer-encoding", "").lower() == "chunked":
            raw, body, buf = chunked(conn, buf)
        else:
            length = int(fields.get("content-length", "0"))
            buf = fill(conn, buf, lambda b: len(b) >= length)
            raw, body, buf = buf[:length], buf[:length], buf[length:]
        path, _, query = target.partition("?")
        endpoint = "info-refs" if path.endswith("/info/refs") else path.rsplit("/", 1)[-1]
        with LOCK:
            COUNT[0] += 1
            name = "%02d-%s-%s" % (COUNT[0], method.lower(), endpoint)
        save(out, name + ".request-head.bin", head + b"\r\n\r\n")
        record_request(out, name, path, fields, raw, body)
        env = {
            "PATH": os.environ["PATH"],
            "HOME": os.environ["HOME"],
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_PROJECT_ROOT": root,
            "GIT_HTTP_EXPORT_ALL": "1",
            "REQUEST_METHOD": method,
            "PATH_INFO": path,
            "QUERY_STRING": query,
            "REMOTE_ADDR": "127.0.0.1",
            "SERVER_PROTOCOL": "HTTP/1.1",
            "CONTENT_LENGTH": str(len(body)),
        }
        for field, variable in (
            ("content-type", "CONTENT_TYPE"),
            ("content-encoding", "HTTP_CONTENT_ENCODING"),
            ("git-protocol", "HTTP_GIT_PROTOCOL"),
        ):
            if field in fields:
                env[variable] = fields[field]
        cgi = subprocess.run(["git", "http-backend"], input=body, capture_output=True, env=env).stdout
        cgi_head, _, cgi_body = cgi.partition(b"\r\n\r\n")
        save(out, name + ".reply-head.bin", cgi_head + b"\r\n\r\n")
        if method == "GET" and path.endswith("/info/refs"):
            save(out, name + ".reply-body.bin", cgi_body)
        status, kept = b"200 OK", []
        for line in cgi_head.split(b"\r\n"):
            if line.lower().startswith(b"status:"):
                status = line.split(b":", 1)[1].strip()
            elif line:
                kept.append(line + b"\r\n")
        reply = b"HTTP/1.1 " + status + b"\r\n" + b"".join(kept)
        conn.sendall(reply + b"Content-Length: %d\r\n\r\n" % len(cgi_body) + cgi_body)


def main():
    portfile, out, root = sys.argv[1:4]
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(8)
    with open(portfile + ".tmp", "w") as f:
        f.write(str(listener.getsockname()[1]))
    os.rename(portfile + ".tmp", portfile)
    while True:
        conn, _ = listener.accept()
        threading.Thread(target=serve, args=(conn, out, root), daemon=True).start()


if __name__ == "__main__":
    main()
