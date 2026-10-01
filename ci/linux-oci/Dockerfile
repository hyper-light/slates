# The Linux container profile's lane (AUD-29-67/74/78; Ada authorized user_allow_other on the CI runner,
# 2026-10-01): the toolchain image plus a Docker engine (run inside, privileged, so a container's bind sees the
# daemon's FUSE mount) and FUSE with `user_allow_other` granted — the operator's setting the shared mount needs.
FROM rust:1.98.0
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends docker.io docker-cli fuse3 strace sudo >/dev/null \
 && rm -rf /var/lib/apt/lists/* && echo user_allow_other >> /etc/fuse.conf
