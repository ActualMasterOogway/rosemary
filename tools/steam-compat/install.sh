#!/bin/sh
# Install Rosemary as a Steam compatibility tool.
#
#   install.sh [--uninstall] [--steam-root DIR] [--rosemary PATH]
#
# The tool calls the rosemary binary found now (PATH, or --rosemary), by
# absolute path. Restart Steam afterwards, then pick "Rosemary (macOS)" under a
# game's Properties > Compatibility.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
steam_root=
rosemary=
uninstall=

while [ $# -gt 0 ]; do
    case "$1" in
    --uninstall) uninstall=1 ;;
    --steam-root) steam_root=${2:?}; shift ;;
    --rosemary) rosemary=${2:?}; shift ;;
    *) echo "usage: $0 [--uninstall] [--steam-root DIR] [--rosemary PATH]" >&2; exit 2 ;;
    esac
    shift
done

if [ -z "$steam_root" ]; then
    for c in "$HOME/.steam/root" "$HOME/.local/share/Steam" "$HOME/.steam/steam"; do
        [ -d "$c" ] && { steam_root=$c; break; }
    done
fi
[ -n "$steam_root" ] || { echo "install.sh: Steam root not found; use --steam-root" >&2; exit 1; }

dest="$steam_root/compatibilitytools.d/Rosemary"

if [ -n "$uninstall" ]; then
    rm -rf "$dest"
    echo "removed $dest"
    exit 0
fi

if [ -z "$rosemary" ]; then
    rosemary=$(command -v rosemary) || { echo "install.sh: rosemary not on PATH; use --rosemary" >&2; exit 1; }
fi
rosemary=$(readlink -f "$rosemary")
[ -x "$rosemary" ] || { echo "install.sh: $rosemary is not executable" >&2; exit 1; }

mkdir -p "$dest"
cp "$here/compatibilitytool.vdf" "$here/toolmanifest.vdf" "$dest/"
cp "$here/rosemary-steam" "$dest/"
chmod +x "$dest/rosemary-steam"

{
    echo "ROSEMARY_BIN='$rosemary'"
    # The Nix package may not ship the generated Steam bridge; use a checkout's.
    client="$repo/tools/steamclient/generated/steamclient.dylib"
    [ -f "$client" ] && echo "ROSEMARY_STEAM_CLIENT='$client'; export ROSEMARY_STEAM_CLIENT"
} >"$dest/rosemary-steam.env"

echo "installed to $dest (rosemary: $rosemary)"
echo "restart Steam to pick it up"
