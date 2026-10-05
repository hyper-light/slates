#!/bin/bash
set -u
cp /target/release/slates /usr/local/bin/slates
slates --instance conf anchor --quick --shards 1 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 200); do slates --instance conf volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance conf bootstrap root >/out/bootstrap.out 2>&1
slates --instance conf mcp --http 0 2>/out/mcp.err &
for _ in $(seq 1 100); do grep -q "Bearer" /out/mcp.err 2>/dev/null && break; sleep 0.1; done
PORT=$(sed -n 's#.*127.0.0.1:\([0-9]*\)/.*#\1#p' /out/mcp.err | head -1)
TOKEN=$(sed -n 's#.*Bearer \([0-9a-f]*\)).*#\1#p' /out/mcp.err | head -1)
echo "edge port=$PORT token-len=${#TOKEN}"
node /conf/proxy.js "$PORT" "$TOKEN" 3000 &
sleep 1
if [ -n "${CONF_SRC:-}" ]; then
  (cd /root && git clone -q --depth 1 https://github.com/modelcontextprotocol/conformance.git && cd conformance \
    && echo "suite at $(git log -1 --format='%h %ci')" && npm ci --silent >/dev/null 2>&1 && npm run build --silent >/dev/null 2>&1)
  RUN="node /root/conformance/dist/index.js"
else
  RUN="npx -y @modelcontextprotocol/conformance"
fi
cd /out && $RUN server --url http://127.0.0.1:3000/mcp -o /out/results ${CONF_ARGS:---suite all} 2>&1 | tail -120
