#!/bin/sh
# Reproduce a Unity/Mono steady state's control flow without Unity, and count what
# it costs the lifter.
#
# A Subnautica steady state spends its VM entries on indirect calls (50%) and
# returns out of a region (31%) - transfers that a per-thread link table should
# answer without an exit. `gen.py` generates the shapes behind them, read off
# Subnautica's own binaries:
#
#   * Mono JIT code, emitted at run time into a MAP_JIT mapping, reaching the
#     runtime the way its amd64 backend does past 2 GiB: `movabs r11; call r11` to
#     `mono_tls_get_lmf_addr` stand-ins, the `GC_dirty_inner` write barrier, and
#     icalls. Densely packed, as the code manager packs it.
#   * Managed virtual calls, `call [rax+8]`.
#   * UnityPlayer-like icalls that dispatch through C++ vtables, scattered through
#     ~12 MiB of text the way UnityPlayer's hot functions sit in its 24 MiB.
#
# The checksum is the same natively (Rosetta) and under `--emulated`; a mismatch is
# a correctness bug, not noise.
#
#     tools/unitybench/unitybench.sh build              generate and compile
#     tools/unitybench/unitybench.sh native [frames]    run it under Rosetta
#     tools/unitybench/unitybench.sh emulated [frames]  run it through Microlift
#     tools/unitybench/unitybench.sh sweep [runs]       entries per frame, per link geometry
#
# Where a run lands depends on ASLR - where the JIT mapping falls relative to the
# image, modulo the link table's span - so judge a link-table change by `sweep`,
# which repeats each configuration, never by a single run. `GEN_ARGS` passes
# `<managed classes> <icalls> <native classes> <pad bytes>` to gen.py.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
out=${UNITYBENCH_DIR:-$repo/target/unitybench}
bin=$out/unitybench
rosemary=${ROSEMARY:-$repo/target/release/rosemary}

build() {
    mkdir -p "$out"
    # shellcheck disable=SC2086
    python3 -I "$here/gen.py" "$out/unitybench.c" ${GEN_ARGS:-}
    cc -arch x86_64 -O2 -o "$bin" "$out/unitybench.c"
    echo "built $bin"
}

need() {
    [ -x "$bin" ] || build
}

# One run's region entries per frame: warm-up included, so a run with no steady
# exits reads (first-frame entries) / frames.
entries_per_frame() {
    frames=$1
    shift
    env "$@" ROSEMARY_STATS=1 "$rosemary" run --emulated "$bin" "$frames" 2>"$out/sweep.err" |
        sed 's/.*steady \([0-9.]*\) ms.*/\1/' >"$out/sweep.ms"
    entries=$(grep 'region entr' "$out/sweep.err" | tail -1 | awk '{print $3}')
    echo "$((entries / frames))/$(cat "$out/sweep.ms")ms"
}

case "${1:-}" in
build)
    build
    ;;
native)
    need
    "$bin" "${2:-200}"
    ;;
emulated)
    need
    ROSEMARY_STATS=${ROSEMARY_STATS:-1} "$rosemary" run --emulated "$bin" "${2:-200}"
    ;;
sweep)
    need
    runs=${2:-10}
    for geometry in "ROSEMARY_LINK_ROWS=8192 ROSEMARY_LINK_WAYS=1" \
        "ROSEMARY_LINK_ROWS=65536 ROSEMARY_LINK_WAYS=1" \
        "ROSEMARY_LINK_ROWS=65536 ROSEMARY_LINK_WAYS=2" \
        "ROSEMARY_LINK_ROWS=65536 ROSEMARY_LINK_WAYS=4"; do
        printf '%s:' "$geometry"
        i=0
        while [ "$i" -lt "$runs" ]; do
            # shellcheck disable=SC2086
            printf ' %s' "$(entries_per_frame 200 $geometry)"
            i=$((i + 1))
        done
        echo
    done
    ;;
*)
    sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
