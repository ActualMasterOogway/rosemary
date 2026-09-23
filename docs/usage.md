# Rosetta 3 CLI Reference

## Environment variables


| Key | Default | Description |
| --- | ------- | ----------- |
| `ROSETTA3_OBJC` | `native` (macOS), `builtin` (other) | Set the Objective-C runtime implementation to use. On Linux, there is only `builtin`. On macOS, `native` uses the real Objective-C, and `builtin` uses the compatibility implementation which can only talk to cross platform reimplementations. `native` yields better compatibility and performance, so use it if you can. It is only recommended you use `builtin` if you are differentially testing, or working upon the compatibility layer from a real macOS system. Make sure that if you're on macOS you also set `ROSETTA3_BUNDLED=1`. |
| `ROSETTA3_BUNDLED` | `0` | (macOS only) - determines if Rosetta 3 should re-execute from a virtual repackage of the application. Makes the real Objective-C runtime recognize plist registrations, unnecessary with `builtin` (and possibly problematic). |
| `ROSETTA3_DEAD_FLAGS` | `0` | Enable Microlift's dead flag elimination pass. Improves performance in some applications, but not by a wide enough margin. **May have semantic accuracy bugs, use with extreme caution**. |
| `ROSETTA3_CALL_RETURN` | `0` | Enable Microlift native call return support. Can improve performance in some applications - disabled by default as measurements were inconclusive and it is additional, complex code surface. |
| `ROSETTA3_STRICT_IMPORTS` | `0` | Refuse to run a program that has unbound imports, rather than announcing them on stderr and running anyway. Unbound imports are always announced; this only decides whether they are fatal. |
| `ROSETTA3_UNBOUND_DETAIL` | `40` | How many unbound imports are named one by one before the rest are only counted. Set to `all` for the whole list. A large application has hundreds, so the default keeps the interesting output - what comes *after* the list - on screen; raise it when a fault at address zero has to be matched against a symbol. |
| `ROSETTA3_NO_FOREIGN` | `0` | (macOS only - unsafe) Assume that we have reimplemented every framework this app asks of us, disabling foreign Objective-C support in the reimplementations for a slight performance boost. Assumed to be always on with non-macOS systems, since there are no native Objective C frameworks to conflict with. |
| `ROSETTA3_STEAM` | `1` | Bridge a guest's Steamworks calls to the host's own Steam client, and publish the Mach service `libsteam_api` asks where Steam is. Set to `0` to turn both off, so that a guest asking for Steam is told there is none. Has no effect unless a generated `steamclient.dylib` exists - see `docs/steam.md`. |
| `ROSETTA3_STEAM_CLIENT` | (found) | The generated guest-facing `steamclient.dylib`. Looked for beside the `rosetta3` binary, and then in `tools/steamclient/generated/` above it, which is where a checkout leaves it. |
| `ROSETTA3_STEAM_HOST_CLIENT` | (found) | The host's own `steamclient.so`. Looked for under `$STEAM_COMPAT_CLIENT_INSTALL_PATH`, `~/.steam/sdk64`, `~/.steam/root`, `~/.steam/steam` and `~/.local/share/Steam`. |
| `ROSETTA3_STEAM_TRACE` | unset | Report every bridged Steamworks call on stderr: the dispatch id, the host interface, and the vtable slot it landed on. |
| `ROSETTA3_SWAP_INTERVAL` | `none` | Force a specific swap interval on OpenGL applications. Allows enabling immediate (0), VSync (1), and -1 (Adaptive VSync), along with other V-blank values, allowing unlocked framerate on apps that use VSync. |
| `ROSETTA3_PREFIX` | the default prefix (`off` on macOS) | The prefix a guest runs in: a directory holding a Darwin filesystem, whose `root/` subdirectory is the guest's `/`. Unset, `rosetta3 run` uses `$XDG_DATA_HOME/rosetta3/prefixes/default` (or `$HOME/.local/share/rosetta3/prefixes/default`), creating it on first use. Set it to another path to keep a guest's files separate, or to `off` to leave guest paths alone entirely. See `docs/prefixes.md`. |
| `ROSETTA3_PREFIX_HOME` | `prefix` | Where the guest's home directory is. `prefix` keeps it inside the prefix, so a prefix is one directory to copy, archive or delete. `host` mounts the guest's `/Users/<name>` onto the real `$HOME` instead, for a user who would rather keep one set of documents than two. |
| `ROSETTA3_TRACE_PREFIX` | unset | Report every guest-to-host path translation on stderr, as the pair of spellings. The namespace sits underneath every file call a guest makes, so this is the first thing to reach for when a program behaves differently inside a prefix. |

## Prefixes

A **prefix** is the Darwin filesystem a guest runs in - a directory whose
`root/` is the guest's `/`. Off macOS, `rosetta3 run` makes one at
`~/.local/share/rosetta3/prefixes/default` the first time it is used and runs
every guest in it, because this host has no `/Users`, no `/Applications` and no
`/Library`, and a Mac program told otherwise writes its files into a shape
nothing else agrees about.

```sh
rosetta3 prefix create            # the default prefix, made on first run anyway
rosetta3 prefix create ~/prefixes/game
rosetta3 prefix info              # where it is, and what it says about itself
```

A guest does **not** have to be inside the prefix to run: the app stays wherever
it is and the guest sees it under `/Volumes/Host`, so
`rosetta3 run ~/games/Foo.app/Contents/MacOS/Foo` works with no setup. What
changes is where the guest *writes*: `~/Library/Application Support/...` is the
prefix's, not this host's.

To run a guest in a prefix of its own - separate saves, separate preferences:

```sh
ROSETTA3_PREFIX=~/prefixes/game rosetta3 run ~/games/Foo.app/Contents/MacOS/Foo
```

A prefix made today is a directory shape and nothing more: the system libraries
a guest links against are this runtime's own reimplementations, not files in
`/usr/lib`. Filling the prefix with a verified MiniDarwin userland is a separate
step that does not exist yet, but is planned.

When a guest behaves differently inside a prefix, `ROSETTA3_TRACE_PREFIX=1`
prints every path translation as a pair, which is almost always enough to
see which one is wrong.

## GUI

Rosetta 3 has a built in GUI that can be toggled with `SHIFT` + `F7`. This GUI shows the framerate of the application, and has various profiling utilities built in, useful for narrowing down performance bottlenecks.

## Execution modes

`rosetta3 run --objc-debug <mach-o> [args...]` enables expensive, fail-fast
checks in the built-in Objective-C runtime. It adds allocation tail guards,
quarantines finalized objects, and validates reference counting, weak slots,
autorelease pools, synchronization, selectors, and class handles. The option
requires `ROSETTA3_OBJC=builtin`; it is intended for diagnosis rather than
normal execution and can substantially increase memory use.

`rosetta3 prefix create [PREFIX]` makes a prefix, and `rosetta3 prefix info
[PREFIX]` describes one. Both default to the standard prefix when no path is
given. See the Prefixes section above.

Rosetta 3 has two execution modes currently:

* native - which runs the app natively if it has a slice for your CPU
* emulated - which runs the app under Microlift's x86-64 recompilation.

If not specified at the CLI, Rosetta 3 will automatically try to see if there is a slice available in the app for your native architecture and run it without getting Microlift involved. Otherwise, it will check if it has an emulated implementation for the architecture.

ARM emulation is not supported yet, but the groundwork is laid for it.
