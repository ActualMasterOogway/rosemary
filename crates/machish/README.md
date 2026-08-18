# machish

`machish` is a templatable Mach-O loader and dynamic linker built for high level emulation of MacOS binaries.

It is the Mach-O counterpart to [`selfish`](https://github.com/V3L0C1T13S/selfish), and shares its primary design: that a loader whose every interaction with memory, symbol resolution, and control transfer goes through a trait can serve a compatibility layer as readily as it serves native execution.
