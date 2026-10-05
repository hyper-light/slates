#!/bin/sh
# Builds the slates CLI for Linux in rust:1.98.0 (release), into the slates-linux-target volume.
docker run --rm \
  -v /Users/adalundhe/Projects/slates:/src:ro \
  -v slates-cargo-reg:/usr/local/cargo/registry \
  -v slates-linux-target:/target \
  -e CARGO_TARGET_DIR=/target -e CARGO_HOME=/usr/local/cargo \
  rust:1.98.0 sh -c "cd /src && cargo build --release -p slates-cli $*"
