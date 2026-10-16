#!/bin/sh
# Measure where an emulated Mono program's time goes, one cost at a time.
#
# Frame times from a real Mono game mix every cost together: JIT churn, call
# trampolines, P/Invoke transitions, the GC and the game itself. `Bench.cs` runs
# each of those as its own timed phase, so a phase that is slow under
# `--emulated` and fast natively names the mechanism, and a profile of that
# phase alone names the code.
#
#     tools/monobench/monobench.sh build                 make the bundle
#     tools/monobench/monobench.sh native                run it on the host CPU
#     tools/monobench/monobench.sh emulated              run it through Microlift
#     tools/monobench/monobench.sh profile <phase>       profile one phase, by symbol
#
# Phases: jit-churn loop recursion virtual interface delegate pinvoke alloc
# strings. `BENCH_SCALE=n` multiplies every loop count, which separates a
# steady-state cost (time scales with it) from a one-off one (it does not).
#
# The runtime is a MonoKickstart binary, not Homebrew's `mono`: Homebrew builds
# for the host only, and an Apple silicon host has no x86-64 slice to emulate.
# Kickstart is universal, statically links Mono, and runs `<name>.exe` beside
# itself, which is exactly what FNA games ship. `KICKSTART` names one; the
# default is Terraria's from Steam. Its class libraries are copied beside it.
# Homebrew's `mcs` compiles the benchmark, since IL has no architecture.
#
# The bundle is an `.app` on purpose: Kickstart changes directory to the main
# bundle's resources, and a loose binary is answered with the host process's
# bundle rather than the guest's.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
out=${MONOBENCH_DIR:-$repo/target/monobench}
app=$out/Bench.app/Contents
rosemary=${ROSEMARY:-$repo/target/release/rosemary}
steam="$HOME/Library/Application Support/Steam/steamapps/common/Terraria/Terraria.app/Contents"
kickstart=${KICKSTART:-$steam/MacOS/Terraria.bin.osx}
libraries=${KICKSTART_LIBS:-$(dirname "$kickstart")/../Resources}

build() {
    [ -f "$kickstart" ] || { echo "no Kickstart binary at $kickstart; set KICKSTART" >&2; exit 1; }
    mkdir -p "$app/MacOS" "$app/Resources"
    cp "$kickstart" "$app/MacOS/Bench.bin.osx"
    for f in mscorlib.dll System.dll System.Core.dll Mono.Posix.dll monoconfig monomachineconfig; do
        cp "$libraries/$f" "$app/Resources/"
    done
    mcs -optimize+ -out:"$app/Resources/Bench.exe" "$here/Bench.cs"
    cat > "$app/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>Bench.bin.osx</string>
<key>CFBundleIdentifier</key><string>dev.rosemary.monobench</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
EOF
    echo "built $out/Bench.app"
}

need() {
    [ -f "$app/Resources/Bench.exe" ] || build
}

case "${1:-}" in
build)
    build
    ;;
native)
    need
    "$app/MacOS/Bench.bin.osx"
    ;;
emulated)
    need
    ROSEMARY_STATS=${ROSEMARY_STATS:-1} "$rosemary" run --emulated "$app/MacOS/Bench.bin.osx"
    ;;
profile)
    [ $# -ge 2 ] || { echo "usage: $0 profile <phase>" >&2; exit 2; }
    need
    hist=$out/$2.hist
    BENCH_ONLY=$2 ROSEMARY_PROFILE=${ROSEMARY_PROFILE:-1} ROSEMARY_PROFILE_OUT=$hist \
        "$rosemary" run --emulated "$app/MacOS/Bench.bin.osx" 2>"$out/$2.log"
    python3 -I "$here/profile-symbols.py" "$hist" "$app/MacOS/Bench.bin.osx" "${TOP:-25}"
    echo "full log: $out/$2.log"
    ;;
*)
    sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
