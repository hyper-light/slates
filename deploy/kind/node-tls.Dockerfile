# The KIND export leg's node image (§4.6 "Kubernetes publication without privilege", AUD-29-75; Ada
# authorized it for CI on 2026-10-01): kind's own node image with Debian's ktls-utils. Its `tlshd` is the
# user-space agent the kernel's NFS client hands an RPC-with-TLS handshake to (Linux
# Documentation/networking/tls-handshake.rst); kTLS then carries the records. `mount.nfs` (nfs-common
# 2.8.3) is already in the base image. The client certificate, key and the authority's certificate are minted
# per run and mounted at /etc/slates-tls by the lane's cluster config, never baked in.
FROM kindest/node:v1.36.4
RUN apt-get update \
  && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ktls-utils \
  && rm -rf /var/lib/apt/lists/*
COPY tlshd.conf /etc/tlshd.conf
# The unit ships `WantedBy=remote-fs.target`, which a kind node never reaches at boot (measured 2026-10-01:
# the enabled service never started, and the mount's handshake upcall failed `ESRCH`). The node reaches
# multi-user.target, so the service is wanted there too.
RUN systemctl enable tlshd.service && systemctl add-wants multi-user.target tlshd.service
