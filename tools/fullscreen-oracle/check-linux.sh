#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
report=$(mktemp)
trap 'rm -f "$report"' EXIT
${CARGO:-cargo} test --offline --release -p appkit --no-run --message-format=json > "$report"
mapfile -t bins < <(python3 - "$report" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    if row.get('reason') == 'compiler-artifact' and row.get('executable'):
        print(row['executable'])
PY
)
((${#bins[@]})) || { echo 'No test executables found' >&2; exit 1; }
xvfb-run -a -s '-screen 0 1440x900x24 -nolisten tcp' bash -euo pipefail -c '
    xfwm4 --compositor=off >/dev/null 2>&1 &
    wm_pid=$!
    trap "kill $wm_pid 2>/dev/null || true" EXIT
    ready=0
    for ((i=0; i<100; i++)); do
        if wmctrl -m >/dev/null 2>&1; then ready=1; break; fi
        sleep 0.05
    done
    ((ready)) || { echo "Window manager did not start" >&2; exit 1; }
    for binary in "$@"; do
        "$binary" --test-threads=1
        case "$binary" in
            */appkit-*)
                "$binary" native_fullscreen_oracle --ignored --test-threads=1 --nocapture
                "$binary" fullscreen_window_replacement_confinement_and_failures --ignored --test-threads=1 --nocapture
                "$binary" fullscreen_transition_posts_geometry_before_did_notifications --ignored --test-threads=1 --nocapture
                "$binary" fractional_fullscreen_frame_matches_content_view --ignored --test-threads=1 --nocapture
                "$binary" opengl_viewport_tracks_fullscreen_logical_frame_and_respects_guest_viewport --ignored --test-threads=1 --nocapture
                ;;
        esac
    done
' bash "${bins[@]}"
