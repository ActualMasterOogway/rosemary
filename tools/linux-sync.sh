#!/bin/sh
# Build this tree for Linux, inside the Alpine dev container.
#
#   tools/linux-sync.sh --vendor        push the vendored dependencies (once)
#   tools/linux-sync.sh                 push the source and `cargo build`
#   tools/linux-sync.sh <command>       push the source and run <command>
#
# Two things about this host are worth knowing before changing it:
#
# * `container cp`, not `-v`. A bind mount wedges the container runtime here: a
#   mounted container never finishes starting, and every container started after
#   it hangs the same way until `container system stop && container system start`.
# * Vendored dependencies, not `cargo fetch`. `microlift` is a private repository
#   and the container has no credentials for it. `cargo vendor` on the Mac turns
#   the whole graph into plain sources, which the container builds offline.
#
# The container keeps its own `CARGO_TARGET_DIR`, so a Linux build never touches
# the macOS `target/`.
#
# One-time container setup:
#
#     container run -d --name r3linux --cpus 8 --memory 8g alpine:latest tail -f /dev/null
#     container exec r3linux apk add rust cargo build-base curl-dev zlib-dev
#     tools/linux-sync.sh --vendor
#
# `curl-dev` and `zlib-dev` are for `shims::curl` and `shims::zlib`, which link
# against the real libraries on either host - they are on the macOS SDK's include
# path and need the `-dev` packages here.
set -e
# macOS `tar` writes an AppleDouble `._name` beside every file with extended
# attributes. `capstone-sys`'s build script globs `*.c`, finds `._MCInst.c`, and
# hands a resource fork to the compiler.
export COPYFILE_DISABLE=1
NAME=${R3_CONTAINER:-r3linux}
VENDOR=${R3_VENDOR:-/tmp/r3vendor}
HERE=$(cd "$(dirname "$0")/.." && pwd)

if [ "$1" = "--vendor" ]; then
    [ -d "$VENDOR" ] || cargo vendor --versioned-dirs "$VENDOR" >/dev/null
    tar -C "$VENDOR" --exclude='._*' -czf /tmp/r3vendor.tgz .
    container exec "$NAME" sh -c 'rm -rf /vendor && mkdir -p /vendor'
    container cp /tmp/r3vendor.tgz "$NAME":/tmp/r3vendor.tgz
    container exec "$NAME" sh -c 'tar -C /vendor -xzf /tmp/r3vendor.tgz'
    echo "vendored sources pushed to $NAME:/vendor"
    exit 0
fi

tar -C "$HERE" -czf /tmp/r3src.tgz \
    --exclude=target --exclude=.git --exclude=.DS_Store --exclude='._*' .
container exec "$NAME" sh -c 'rm -rf /src && mkdir -p /src'
container cp /tmp/r3src.tgz "$NAME":/tmp/r3src.tgz
container exec "$NAME" sh -c 'tar -C /src -xzf /tmp/r3src.tgz'

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

[ $# -eq 0 ] && set -- cargo build --bin rosetta3
container exec "$NAME" sh -c "cd /src && CARGO_TARGET_DIR=/target $*"
