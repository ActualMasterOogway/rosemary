#!/bin/sh
# Register a guest as the desktop handler for one or more URL schemes.
set -eu

applications="${XDG_DATA_HOME:-$HOME/.local/share}/applications"

usage() {
    echo "usage: $0 <mach-o> <scheme>..." >&2
    echo "       $0 --uninstall <mach-o>" >&2
    exit 2
}

[ $# -ge 2 ] || usage

uninstall=
if [ "$1" = "--uninstall" ]; then
    uninstall=1
    shift
fi

guest=$1
shift
[ -n "$uninstall" ] || [ $# -ge 1 ] || usage

# Absolute, because a desktop entry is read from anywhere.
guest=$(cd "$(dirname "$guest")" && pwd)/$(basename "$guest")
name=$(basename "$guest")
entry="$applications/rosemary-$name.desktop"

if [ -n "$uninstall" ]; then
    rm -f "$entry"
    [ -d "$applications" ] && update-desktop-database "$applications" 2>/dev/null || true
    echo "removed $entry"
    exit 0
fi

[ -x "$guest" ] || { echo "$0: $guest: not an executable" >&2; exit 1; }

rosemary=$(cd "$(dirname "$0")/.." && pwd)/target/release/rosemary
[ -x "$rosemary" ] || { echo "$0: build rosemary first: cargo build --release --bin rosemary" >&2; exit 1; }

mime=
for scheme in "$@"; do
    mime="$mime""x-scheme-handler/$scheme;"
done

mkdir -p "$applications"
cat > "$entry" <<ENTRY
[Desktop Entry]
Type=Application
Name=$name (rosemary)
NoDisplay=true
Terminal=false
Exec=$rosemary run $guest -protocolString %u
MimeType=$mime
ENTRY

for scheme in "$@"; do
    xdg-mime default "rosemary-$name.desktop" "x-scheme-handler/$scheme"
done
update-desktop-database "$applications" 2>/dev/null || true

echo "wrote $entry"
echo "handling: $mime"
