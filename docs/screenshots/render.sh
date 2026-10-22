#!/bin/sh
# Renders the README's screenshots, docs/screenshots/NAME.png, from the
# TUI's own frames:
#
#   1. the ignored test screenshots::screenshot_frames (tests/tui_render.rs)
#      draws each scene with the real renderer, the way the TUI tests do, and
#      writes it as 24-bit ANSI into target/screenshots/ (NAME.ans, NAME.txt
#      and the list `scenes`);
#   2. Chromium's headless shell opens frame.html on each frame (xterm.js,
#      JetBrains Mono) once to measure it, then again at that size to save a
#      2x PNG.
#
# Usage: docs/screenshots/render.sh
#
# Needs cargo and chrome-headless-shell: $CHROME if set, else Playwright's
# (`npx playwright install chromium-headless-shell`), else one on the PATH.
# Chrome's own --headless won't do: its page is shorter than its window, so
# the bottom rows come out blank.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
frames="$root/target/screenshots"

find_chrome() {
    if [ -n "${CHROME:-}" ]; then
        printf '%s\n' "$CHROME"
        return
    fi
    found=
    # The last one the globs list, if there are several.
    for c in "$HOME"/.cache/ms-playwright/chromium_headless_shell-*/chrome-headless-shell-*/chrome-headless-shell \
        "$HOME"/Library/Caches/ms-playwright/chromium_headless_shell-*/chrome-headless-shell-*/chrome-headless-shell; do
        if [ -x "$c" ]; then found=$c; fi
    done
    if [ -z "$found" ] && command -v chrome-headless-shell >/dev/null 2>&1; then
        found=$(command -v chrome-headless-shell)
    fi
    if [ -z "$found" ]; then
        echo "render.sh: no chrome-headless-shell; run \`npx playwright install chromium-headless-shell\` or set CHROME" >&2
        exit 1
    fi
    printf '%s\n' "$found"
}

chrome=$(find_chrome)

# Headless, at 2 device pixels per CSS pixel, letting frame.html read the
# frame next to it. Virtual time runs the page until it is idle; without
# --run-all-compositor-stages-before-draw it can run out before the
# terminal's first animation frame, about one time in ten.
headless() {
    "$chrome" --headless --disable-gpu --hide-scrollbars \
        --allow-file-access-from-files --force-device-scale-factor=2 \
        --run-all-compositor-stages-before-draw --virtual-time-budget=10000 \
        "$@" </dev/null 2>/dev/null
}

# The frame's size in CSS pixels, "W,H", once frame.html at URL $1 has
# drawn it; nothing if it hasn't.
measure() {
    headless --dump-dom "$1" | sed -n 's/.*data-size="\([0-9]*,[0-9]*\)".*/\1/p'
}

# Saves frame.html at URL $1 as the PNG $3 in a window of size $2. Fails
# unless the page had drawn the frame (it shows nothing until then) and the
# page is the window's size.
shoot() {
    rm -f "$3"
    dom=$(headless --window-size="$2" --screenshot="$3" --dump-dom "$1")
    case $dom in
    *"data-size=\"$2\""*) ;;
    *) return 1 ;;
    esac
    case $dom in
    *"data-viewport=\"$2\""*) ;;
    *)
        echo "render.sh: $chrome draws a page smaller than its window; use chrome-headless-shell" >&2
        exit 1
        ;;
    esac
    [ -s "$3" ]
}

# frame.html as a file:// URL (escaping what a path may hold that a URL can't).
page="file://$(printf '%s' "$here" | sed 's/%/%25/g; s/ /%20/g; s/#/%23/g; s/?/%3F/g')/frame.html"

BEACON_SCREENSHOTS_DIR="$frames" cargo test --quiet --manifest-path "$root/Cargo.toml" \
    --test tui_render -- --ignored --exact screenshots::screenshot_frames

while read -r name cols rows; do
    url="$page?ans=../../target/screenshots/$name.ans&cols=$cols&rows=$rows"
    png="$here/$name.png"
    # Three tries each: a cold Chromium can run out of time before drawing.
    size=
    for _ in 1 2 3; do
        size=$(measure "$url")
        if [ -n "$size" ]; then break; fi
    done
    shot=
    for _ in 1 2 3; do
        if [ -n "$size" ] && shoot "$url" "$size" "$png"; then
            shot=$png
            break
        fi
    done
    if [ -z "$shot" ]; then
        echo "render.sh: $name: frame.html never drew $name.ans; open $url in Chromium to see why" >&2
        exit 1
    fi
    echo "$name.png  ${cols}x$rows  $(($(wc -c <"$png") / 1024)) KB"
done <"$frames/scenes"
