#!/bin/sh -eu

RUSTFLAGS="-C target-cpu=native" CFLAGS="-march=native" CXXFLAGS="-march=native" cargo build --bin rosemary --profile fullnative