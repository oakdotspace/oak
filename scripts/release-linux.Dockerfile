# Toolchain image for the Linux half of the Oak CI release
# (`scripts/release-oak-ci.sh build-linux`).
#
# fuser's libfuse-free build only works on a Linux host (see the Makefile's
# build-release-linux), so the macOS release runner builds both Linux targets
# in a container from this image on its Docker VM (colima): the same `make
# build-release-linux` the GitHub linux job runs, with the same zig (0.13.0)
# and glibc floor. From an arm64 VM the x86_64 target is the zig cross-link.
#
# The image holds only the toolchain, so it stays cached between releases.
# The source goes into a throwaway container (docker cp), which is removed
# after the binaries are copied out; nothing from a build is kept.
#
# Build context: just rust-toolchain.toml, which pins the channel.
FROM rust:1.94-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends python3-pip \
 && rm -rf /var/lib/apt/lists/* \
 && pip3 install --break-system-packages ziglang==0.13.0 \
 && ln -s "$(python3 -c 'import os, ziglang; print(os.path.dirname(ziglang.__file__))')/zig" /usr/local/bin/zig \
 && cargo install cargo-zigbuild --version 0.23.4 --locked

# Install the pinned toolchain with both Linux targets, so the targets land on
# the toolchain the build actually uses.
COPY rust-toolchain.toml /src/
WORKDIR /src
RUN rustup toolchain install \
      --profile minimal --component rustfmt,clippy \
      --target x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu \
      "$(sed -nE 's/^channel *= *"([^"]+)".*/\1/p' rust-toolchain.toml)"
