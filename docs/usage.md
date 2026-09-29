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
| `ROSETTA3_PREFIX_SETUP` | `ask` | What to do when a prefix needs its Darwin base and it is not cached. `ask` asks - a dialog under `rosetta3 run` when there is a display, then the terminal - and declines when there is nobody to ask. `download` fetches it without asking. `fail` never downloads. The base is verified against hashes compiled into Rosetta 3 whichever route it takes. |
| `ROSETTA3_ROOTFS_CACHE` | `$XDG_CACHE_HOME/rosetta3/rootfs` | Where verified Darwin bases are cached, shared by every prefix. Falls back to `$HOME/.cache/rosetta3/rootfs`. |
| `ROSETTA3_GUEST_PATH` | unset | (internal) Set by a prefixed guest's `exec` of another Mach-O to carry the guest's `PATH` to the child runtime, which gives it to the child guest instead of the default Darwin `PATH`. Never visible to a guest; see `docs/prefixes.md`. |
| `ROSETTA3_TRACE_PREFIX` | unset | Report every guest-to-host path translation on stderr, as the pair of spellings. The namespace sits underneath every file call a guest makes, so this is the first thing to reach for when a program behaves differently inside a prefix. |
| `ROSETTA3_DECORATIONS` | `client` | Who draws a titled window's titlebar. `client` draws a Mac-style titlebar (window buttons, title, and the application's menus) in the window itself, and keeps SDL from loading libdecor, whose GTK 3 plugin cannot share a process with the GTK 4 that WebKit loads. `server` leaves the titlebar to the host's window manager. Applies to the `builtin` AppKit on every host, including macOS with `ROSETTA3_OBJC=builtin`. See `docs/decorations.md`. |
| `ROSETTA3_MENU` | `titlebar` (`client`), `strip` (`server`) | Where the application's main menu is shown. `titlebar` puts it in the titlebar after the window buttons; `strip` puts it in a row of its own under the titlebar, which is the only placement under `server` decorations; `none` shows no menus. Key equivalents work whichever is chosen. |
| `ROSETTA3_MENU_CTRL` | `1` | Let Control stand in for Command in menu key equivalents, so Ctrl+S reaches an item whose equivalent is Command-S. Only when nothing took the key as Control first. Set to `0` to leave Control to the program. |
| `ROSETTA3_CONFINE_POINTER` | unset | Keep the pointer inside the focused window. A workaround, not Mac behavior: some ports (Feral's, e.g. Life Is Strange) confine the cursor with a Win32 `ClipCursor` emulation that clamps only once and counts on warping the pointer back when it strays, which cannot reach a pointer that has left the window on Wayland. Their drag interactions stop short without it. |

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

### The Darwin base

A prefix also holds a **base**: the [MiniDarwin](https://github.com/V3L0C1T13S/minidarwin)
userland - real `/usr/lib` dylibs, the BSD tools in `/bin` and `/usr/bin`, and a
terminfo database. The system libraries a guest *links against* are still this
runtime's own reimplementations; the base is there for a guest that stats,
reads or `dlopen`s a system file, and so that a prefix has a real Darwin
filesystem shape. A prefix without one still runs guests.

A new prefix gets its base when it is created, and nothing is downloaded
without consent: `rosetta3 run` asks with a dialog or on the terminal, and
`ROSETTA3_PREFIX_SETUP` answers in advance. The download (about 2 MB) is
checked against hashes compiled into this build of Rosetta 3, and kept in a
cache so that every other prefix, and every repair, works offline.

```sh
rosetta3 prefix create                        # shape, then base for this host's architecture
rosetta3 prefix create --arch x86_64          # the base for a given guest architecture
rosetta3 prefix create --bundle B.bundle.zip  # offline, from a MiniDarwin release bundle
rosetta3 prefix create --spec S.spec.yaml     # a MiniDarwin image this build does not pin
rosetta3 prefix create --system-files immutable
rosetta3 prefix create --no-base              # the shape only

rosetta3 prefix verify         # hash every system file against the manifest
rosetta3 prefix repair         # put back what differs, offline (--dry-run to preview)
rosetta3 prefix reset-system   # also moves aside what a user put in a system directory
rosetta3 prefix update         # move to the release this build pins (--allow-downgrade)
rosetta3 prefix cache list     # cached bases, and which prefixes use them
rosetta3 prefix cache gc       # remove the ones nothing uses
```

`--bundle` checks the bundle against the pinned spec (or `--spec`), never
against the spec inside it. That file travelled with what it vouches for.

`--system-files` decides what happens when a guest writes a file the base owns:

* `repair` (the default) - allowed, and restored at the guest's next start;
* `warn` - allowed, and reported at every start until `prefix repair`;
* `immutable` - refused with `EROFS`. The base's files are then hardlinked
  from the cache where the filesystem cannot clone them.

Nothing a user made is ever deleted. `repair` and `reset-system` move
anything in the way to `<prefix>/.rosetta3/displaced/`.

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

`rosetta3 prefix <create|info|verify|repair|reset-system|update> [PREFIX]`
manages a prefix, defaulting to the standard one when no path is given, and
`rosetta3 prefix cache <list|gc>` the cache of Darwin bases. See the Prefixes
section above.

Rosetta 3 has two execution modes currently:

* native - which runs the app natively if it has a slice for your CPU
* emulated - which runs the app under Microlift's x86-64 recompilation.

If not specified at the CLI, Rosetta 3 will automatically try to see if there is a slice available in the app for your native architecture and run it without getting Microlift involved. Otherwise, it will check if it has an emulated implementation for the architecture.

ARM emulation is not supported yet, but the groundwork is laid for it.
