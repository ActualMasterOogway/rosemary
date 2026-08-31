# Rosetta 3 CLI Reference

## Environment variables


| Key | Default | Description |
| --- | ------- | ----------- |
| `ROSETTA3_OBJC` | `native` (macOS), `builtin` (other) | Set the Objective-C runtime implementation to use. On Linux, there is only `builtin`. On macOS, `native` uses the real Objective-C, and `builtin` uses the compatibility implementation which can only talk to cross platform reimplementations. `native` yields better compatibility and performance, so use it if you can. It is only recommended you use `builtin` if you are differentially testing, or working upon the compatibility layer from a real macOS system. Make sure that if you're on macOS you also set `ROSETTA3_BUNDLED=1`. |
| `ROSETTA3_BUNDLED` | `0` | (macOS only) - determines if Rosetta 3 should re-execute from a virtual repackage of the application. Makes the real Objective-C runtime recognize plist registrations, unnecessary with `builtin` (and possibly problematic). |
| `ROSETTA3_DEAD_FLAGS` | `0` | Enable Microlift's dead flag elimination pass. Improves performance in some applications, but not by a wide enough margin. **May have semantic accuracy bugs, use with extreme caution**. |
| `ROSETTA3_CALL_RETURN` | `0` | Enable Microlift native call return support. Can improve performance in some applications - disabled by default as measurements were inconclusive and it is additional, complex code surface. |
| `ROSETTA3_STRICT_IMPORTS` | `0` | Refuse to run a program that has unbound imports, rather than announcing them on stderr and running anyway. Unbound imports are always announced; this only decides whether they are fatal. |

## GUI

Rosetta 3 has a built in GUI that can be toggled with `SHIFT` + `F7`. This GUI shows the framerate of the application, and has various profiling utilities built in, useful for narrowing down performance bottlenecks.

## Execution modes

Rosetta 3 has two execution modes currently:

* native - which runs the app natively if it has a slice for your CPU
* emulated - which runs the app under Microlift's x86-64 recompilation.

If not specified at the CLI, Rosetta 3 will automatically try to see if there is a slice available in the app for your native architecture and run it without getting Microlift involved. Otherwise, it will check if it has an emulated implementation for the architecture.

ARM emulation is not supported yet, but the groundwork is laid for it.
