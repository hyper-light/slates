// The conformance harness cannot present the slates edge's bearer token, so this proxy adds it. It maps only its own
// address to the edge's (Host and Origin naming 127.0.0.1:LISTEN or localhost:LISTEN become the edge's); any other
// Host or Origin passes through unchanged, so the edge's own rebinding checks (AUD-29-23) see what the harness sent.
const http = require('http');
const [port, token, listen] = process.argv.slice(2);
const ours = new Set([`127.0.0.1:${listen}`, `localhost:${listen}`]);
http.createServer((req, res) => {
  const headers = { ...req.headers, authorization: `Bearer ${token}` };
  if (ours.has(headers.host)) headers.host = `127.0.0.1:${port}`;
  if (headers.origin) {
    try {
      const origin = new URL(headers.origin);
      if (ours.has(origin.host)) headers.origin = `http://127.0.0.1:${port}`;
    } catch (_) { /* a malformed Origin passes through for the edge to refuse */ }
  }
  const upstream = http.request({ host: '127.0.0.1', port, path: req.url, method: req.method, headers }, (up) => {
    res.writeHead(up.statusCode, up.headers);
    up.pipe(res);
  });
  upstream.on('error', (e) => { res.writeHead(502); res.end(String(e)); });
  req.pipe(upstream);
}).listen(Number(listen), '127.0.0.1');
