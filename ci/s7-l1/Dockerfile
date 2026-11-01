# The arm64 KVM host for the DAX matrix's Linux arm64 cells (Step 7 of the VFS and DAX plan), booted as a
# nested-virtualization VM on an Apple M-series Mac (vzrun, docs/wip/arm64-kvm-on-a-mac.md). Built on CI's native arm64
# runner from a diagnostics branch, never on the Mac, which runs no Docker. Built from the slates tree as context, with
# this folder as the `l1` build context.
FROM rust:1.98.1 AS build
WORKDIR /work
COPY . .
ENV RUSTUP_TOOLCHAIN=1.98.1 CARGO_BUILD_JOBS=4
RUN mkdir -p /out/work/target/release /out/opt/slates \
 && cargo build --release --locked -p slates-cli --bin slates \
 && cp target/release/slates /out/work/target/release/slates \
 && cargo test --release --locked -p slates-cli --test guest_image --test guest_vhost --no-run --message-format=json > /tmp/test.json \
 && for t in guest_image guest_vhost; do \
      T=$(grep -o "\"executable\":\"[^\"]*/${t}-[^\"]*\"" /tmp/test.json | head -1 | cut -d'"' -f4); \
      echo "test binary $t: $T"; cp "$T" "/out/opt/slates/$t"; \
    done \
 && sed -i 's/slates.dax=always slates.modules/slates.dax=never slates.modules/' crates/cli/tests/guest_image.rs \
 && grep -q 'slates.dax=never slates.modules' crates/cli/tests/guest_image.rs \
 && cargo test --release --locked -p slates-cli --test guest_image --no-run --message-format=json > /tmp/red.json \
 && T=$(grep -o '"executable":"[^"]*/guest_image-[^"]*"' /tmp/red.json | head -1 | cut -d'"' -f4) \
 && echo "red test binary: $T" && cp "$T" /out/opt/slates/guest_image_red \
 && rustup target add aarch64-unknown-linux-musl \
 && RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --locked -p slates-guest-probe --bin slates-guest-probe --target aarch64-unknown-linux-musl \
 && cp target/aarch64-unknown-linux-musl/release/slates-guest-probe /out/opt/slates/slates-guest-probe

# crosvm at the commit the x86_64 box runs (1c8d17a85), with its own toolchain (its rust-toolchain: 1.88.0), built
# without its default features (gpu, audio, usb, net, wl): pmem, the vhost-user front end and the fs device are core.
FROM debian:trixie AS crosvm
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends ca-certificates curl git build-essential \
      pkg-config libcap-dev clang libclang-dev >/dev/null
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.88.0
ENV PATH=/root/.cargo/bin:$PATH
RUN git clone --filter=blob:none https://chromium.googlesource.com/crosvm/crosvm /crosvm \
 && cd /crosvm && git checkout 1c8d17a85 && git submodule update --init third_party/minijail
RUN cd /crosvm && cargo build --release --no-default-features && cp target/release/crosvm /crosvm-bin \
 && /crosvm-bin version

FROM debian:trixie-slim AS fetch
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends curl ca-certificates >/dev/null
WORKDIR /fetch
RUN curl -fsSLO https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/firecracker-v1.17.0-aarch64.tgz \
 && echo "e351ebe4f7a16b5873bbd51005d2e6767103cff4d5ebc829df2d3f95a93e2256  firecracker-v1.17.0-aarch64.tgz" | sha256sum -c \
 && tar -xzf firecracker-v1.17.0-aarch64.tgz && cp release-v1.17.0-aarch64/firecracker-v1.17.0-aarch64 /fetch/firecracker \
 && curl -fsSLO https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v54.0/cloud-hypervisor-static-aarch64 \
 && echo "e8a6d9786ea678e739f1a386682846e93742dcb9c7b6e3e0c62e3bda6cff0ef3  cloud-hypervisor-static-aarch64" | sha256sum -c \
 && chmod 755 cloud-hypervisor-static-aarch64

FROM debian:trixie-slim
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends linux-image-arm64 e2fsprogs fuse3 kmod \
      busybox-static procps coreutils libcap2 >/dev/null 2>&1 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/ /
COPY --from=crosvm /crosvm-bin /opt/crosvm/crosvm
COPY --from=fetch /fetch/firecracker /opt/firecracker/firecracker
COPY --from=fetch /fetch/cloud-hypervisor-static-aarch64 /opt/cloud-hypervisor/cloud-hypervisor
COPY --from=l1 init /init
# crosvm's sandbox pivots each jailed device into /var/empty, which a slim image lacks.
RUN chmod 755 /init && mkdir -p /var/empty
