#!/bin/sh
# Record an Instruments trace of a guest program, with the map that makes it
# readable.
#
# The two halves have to come from the *same run*. A trace records host
# addresses in whatever anonymous mappings the JIT got that time, and the map
# says what those addresses were lifted from; run them separately and the
# addresses in one mean nothing in the other. So this launches the guest under
# `xctrace` with `ROSETTA3_PROFILE_MAP` set, and both land beside each other.
#
#     tools/guest-trace.sh [-t TEMPLATE] [-o OUT] <guest> [args...]
#
# Then:
#
#     tools/guest-symbolicate.py OUT.trace OUT.map
#
# Templates worth asking for, and what each answers:
#
#   Time Profiler     the host-side call tree. Joins. This is the verified path.
#   CPU Counters      cycles, instructions retired, branch misses, cache misses.
#   Processor Trace   records, and does NOT join: its locations are (binary
#                     UUID, Mach-O offset) pairs rather than runtime addresses,
#                     so every translation folds into one unnamed sentinel. It
#                     also needs Enable Processor Trace in System Settings ->
#                     Privacy & Security -> Developer Tools, and the driver
#                     re-signed with tools/get-task-allow.plist. See
#                     docs/PROFILING.md before spending time on it.
#
# None of them symbolicate JIT code, which is the whole reason the map exists.

set -eu

template='Time Profiler'
out=''
while [ $# -gt 0 ]; do
    case "$1" in
        -t|--template) template="$2"; shift 2 ;;
        -o|--output) out="$2"; shift 2 ;;
        --) shift; break ;;
        -*) echo "unknown option: $1" >&2; exit 2 ;;
        *) break ;;
    esac
done

[ $# -ge 1 ] || { echo "usage: $0 [-t TEMPLATE] [-o OUT] <guest> [args...]" >&2; exit 2; }

guest="$1"; shift
[ -n "$out" ] || out="$(basename "$guest").$$"

# `xctrace` needs the *full* Xcode to be the selected developer directory. On a
# machine with the Command Line Tools selected — which is the ordinary state,
# and this one — `/usr/bin/xctrace` exists, is executable, and fails with
# "requires Xcode". So presence is not the test: running it is.
xctrace=''
for candidate in xctrace /Applications/Xcode.app/Contents/Developer/usr/bin/xctrace; do
    if "$candidate" version >/dev/null 2>&1; then xctrace="$candidate"; break; fi
done
[ -n "$xctrace" ] || { echo "no working xctrace: install Xcode (the app, not just the Command Line Tools)" >&2; exit 1; }

root="$(cd "$(dirname "$0")/.." && pwd)"
driver="${ROSETTA3:-$root/target/release/rosetta3}"
[ -x "$driver" ] || { echo "no driver at $driver: cargo build --release" >&2; exit 1; }

echo "recording '$template' -> $out.trace, map -> $out.map"
rm -rf "$out.trace"

# `--env` reaches the launched process, which is the driver, which is what
# writes the map. Instruction-level detail is what a branch trace wants: a
# region-level map answers "which function", and Processor Trace is recording
# every branch inside one.
"$xctrace" record \
    --template "$template" \
    --output "$out.trace" \
    --target-stdout - \
    --env "ROSETTA3_PROFILE_MAP=$out.map" \
    --env "ROSETTA3_PROFILE_MAP_INSTRUCTIONS=1" \
    --env "ROSETTA3_PROFILE=${ROSETTA3_PROFILE:-1}" \
    --env "ROSETTA3_PROFILE_OUT=$out.profile" \
    --launch -- "$driver" run "$guest" "$@"

echo
echo "wrote $out.trace, $out.map, $out.profile"
echo "join them: $(dirname "$0")/guest-symbolicate.py $out.trace $out.map"
