# Rosetta 3

Rosetta 3 is a high level emulator for running x86-64 and AArch64 macOS binaries on macOS and Linux. It implements a compliant Mach-O loader using `machish` as its base, and shims the x86-64 Mac ABI to the AArch64 one, additionally shimming Darwin APIs to cross platform libc ones where necessary.

Two backends are available on either host:

* native, which links the program against the host's own dylibs and calls it directly. On an x86-64 host this is the backend selection picks for any x86-64 binary, because translating a program into the instruction set it is already written in buys nothing.
* emulated, which recompiles the program through Microlift.

Build either with `cargo build --release --target x86_64-apple-darwin --bin rosetta3`; leaving the target off builds for the machine you are on.

Rosetta 3 builds its CPU emulation using the Microlift binary lifter as a base.

## Performance

Rosetta 3 is relatively fast, though not up to speed with Rosetta 2 in all cases. Rosetta 2 largely outperforms Rosetta 3 in binaries that make heavy use of Objective C, as our trampoline layer for Objective C shimming carries quite a bit of overhead at the moment. We are working out quirks and investigating alternatives that will allow us to achieve equivalent performance from Rosetta 2.

Rosetta 3, like many other high level emulators, implements Block Linking, a feature that makes jumping between different unknown blocks at lift time possible directly in the recompiled code via a jump table and a pinned register ABI. Microlift exposes the core of this functionality, and Rosetta 3 acts as the runtime that implements it.

Rosetta 3 can optionally use Microlift's automatic flag analysis to optimize overflow and carry flag calculation via native hardware support. It is disabled by default out of accuracy concerns, following the same suit as Microlift (accuracy first, performance second). In most applications, leaving it off should not have a substantial performance impact.

## Running on Linux

Rosetta 3 builds and runs on Linux via the Darwin shim crate. It works by reimplementing Darwin APIs where they diverge on Linux, and passing the rest of them through to the host.

Note: The Objective-C runtime, libdispatch, the Mach interfaces, the debug overlay and the `.app` bundle layout are macOS-only and are compiled out elsewhere. They are not implemented yet.

`tools/linux-sync.sh` builds and runs the tree inside an Alpine container; its header documents the one-time setup.

## Differences from Rosetta 2

Rosetta 3 diverges from Rosetta 2 in a few ways at the moment - in particular, these are the differences that will likely be most impactful:

* Rosetta 3 uses a full on compiler IR rather than direct translation - this can improve code quality, however, it increases lift time by a large margin.
* Rosetta 3 does not support AOT recompilation yet. There are complications involving how to link a cached AOT object back into the address space without losing efficiency from having to convert direct IP-relative instructions to `image_base + offset` - we plan to possibly make all memory references link-time variables that we can relocate when loading up the cached AOT recompiled basic blocks.
* Rosetta 3 does not use x86-64 system libraries like Rosetta 2 does. All libraries, from the libc to the Objective C runtime, OpenGL, and libdispatch, are all bridged with Microlift's fast import system, which allows direct calling of host ARM functions. This gives us a slight edge over Rosetta 2 in terms of on-disk size, given how our job is to just bridge to existing ARM ports of system libraries rather than recompile them. The exception to this is `libc++` libraries, which contain ABI differences between the x86-64 ABI and the AArch64 one. Bridging these differences is technically possible, however, the C++ ABI is a bit unstable and we would rather not delve into that mess. Most C++ functions call out to host libc functions anyway, so the performance impact is minimal.
