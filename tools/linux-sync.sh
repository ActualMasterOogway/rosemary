#!/bin/sh
# Build this tree for Linux, inside the Debian dev container.
#
#   tools/linux-sync.sh --vendor        push the vendored dependencies (once)
#   tools/linux-sync.sh                 push the source and `cargo build`
#   tools/linux-sync.sh <command>       push the source and run <command>
#
# Three things about this host are worth knowing before changing it:
#
# * `container cp`, not `-v`. A bind mount wedges the container runtime here: a
#   mounted container never finishes starting, and every container started after
#   it hangs the same way until `container system stop && container system start`.
# * Vendored dependencies, not `cargo fetch`. `microlift` is a private repository
#   and the container has no credentials for it. `cargo vendor` on the Mac turns
#   the whole graph into plain sources, which the container builds offline.
# * Debian, not Alpine. Alpine has no SDL3 and its X11 and Mesa packaging is a
#   fight; the frameworks this tree reimplements are built on both. The cost is
#   glibc instead of musl, so musl is no longer covered by this loop.
#
# The toolchain is rustup's, not Debian's: trixie ships rustc 1.85 and this tree
# needs stable C variadics, which require 1.99.
#
# The container keeps its own `CARGO_TARGET_DIR`, so a Linux build never touches
# the macOS `target/`.
#
# One-time container setup:
#
#     container run -d --name rosemary-debian --cpus 8 --memory 8g debian:trixie tail -f /dev/null
#     container exec rosemary-debian sh -c 'apt-get update -qq && \
#         DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
#         build-essential curl pkg-config libcurl4-openssl-dev zlib1g-dev libsdl3-dev \\
#         libvulkan1 mesa-vulkan-drivers libxkbcommon-dev'
#     container exec rosemary-debian sh -c 'curl --proto "=https" --tlsv1.2 -sSf \
#         https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.99.0 --profile minimal'
#     tools/linux-sync.sh --vendor
#
# `libcurl4-openssl-dev` and `zlib1g-dev` are for `shims::curl` and `shims::zlib`,
# which link against the real libraries on either host - they are on the macOS
# SDK's include path and need the `-dev` packages here. `libsdl3-dev` is what the
# reimplemented AppKit, IOKit and CoreAudio are built on, and it brings libGL with
# it. Built-in GLU compatibility uses SDL-resolved GL entry points too.
# `libvulkan1` is Metal's loader and `mesa-vulkan-drivers` supplies an ICD,
# including lavapipe for the container's headless adapter-discovery tests.
# `libxkbcommon-dev` is linked by smithay, the compositor WebKitGTK's windows
# live in; see docs/webkit.md.
#
# There is no display in the container. SDL runs headless by default; see the
# `SDL_*` variables set below.
set -e
# macOS `tar` writes an AppleDouble `._name` beside every file with extended
# attributes. `capstone-sys`'s build script globs `*.c`, finds `._MCInst.c`, and
# hands a resource fork to the compiler.
export COPYFILE_DISABLE=1
NAME=${ROSEMARY_CONTAINER:-rosemary-debian}
VENDOR=${ROSEMARY_VENDOR:-/tmp/rosemary-vendor}
HERE=$(cd "$(dirname "$0")/.." && pwd)

if [ "$1" = "--vendor" ]; then
    [ -d "$VENDOR" ] || cargo vendor --versioned-dirs "$VENDOR" >/dev/null
    tar -C "$VENDOR" --exclude='._*' -czf /tmp/rosemary-vendor.tgz .
    container exec "$NAME" sh -c 'rm -rf /vendor && mkdir -p /vendor'
    container cp /tmp/rosemary-vendor.tgz "$NAME":/tmp/rosemary-vendor.tgz
    container exec "$NAME" sh -c 'tar -C /vendor -xzf /tmp/rosemary-vendor.tgz'
    echo "vendored sources pushed to $NAME:/vendor"
    exit 0
fi

tar -C "$HERE" -czf /tmp/rosemary-src.tgz \
    --exclude=target --exclude=.git --exclude=.DS_Store --exclude='._*' .
container exec "$NAME" sh -c 'rm -rf /src && mkdir -p /src'
container cp /tmp/rosemary-src.tgz "$NAME":/tmp/rosemary-src.tgz
# `-m`: give every extracted file the current time rather than the one the Mac
# recorded. `container cp` is not a mount, so cargo compares a *fresh* source
# tree against artifacts built from the previous push, and a source file whose
# mtime is older than the artifact is one cargo does not rebuild - which shows up
# as an edit that had no effect and a test that fails against code that is no
# longer there.
container exec "$NAME" sh -c 'tar -m -C /src -xzf /tmp/rosemary-src.tgz'

# Container-side only: the host tree keeps its own `.cargo/config.toml`.
container exec "$NAME" sh -c 'mkdir -p /src/.cargo && cat > /src/.cargo/config.toml <<CFG
[source.crates-io]
replace-with = "vendored-sources"

[source."git+https://github.com/microlift/function_traits.git"]
git = "https://github.com/microlift/function_traits.git"
replace-with = "vendored-sources"

[source."git+https://github.com/microlift/microlift"]
git = "https://github.com/microlift/microlift"
replace-with = "vendored-sources"

[source."git+https://github.com/microlift/wasmtime?branch=microlift_experimental"]
git = "https://github.com/microlift/wasmtime"
branch = "microlift_experimental"
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "/vendor"
CFG'

[ $# -eq 0 ] && set -- cargo build --bin rosemary
# No display and no sound card. SDL is told so rather than left to fail at the
# first `SDL_Init`, and the joystick driver is the virtual one so the HID layer
# has something deterministic to enumerate.
container exec "$NAME" sh -c ". \$HOME/.cargo/env && cd /src && \
    CARGO_TARGET_DIR=/target \
    SDL_VIDEODRIVER=\${SDL_VIDEODRIVER:-offscreen} \
    SDL_AUDIODRIVER=\${SDL_AUDIODRIVER:-dummy} \
    $*"
