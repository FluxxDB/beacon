#!/usr/bin/env python3
"""Extract the approved TUI mockup screens into test fixtures.

docs/design/tui-mockup.html ends with a <script> that defines
`const SCREENS = {...};`, keyed by screen name. Each value is
{"ansi": <SGR-escaped text, rows separated by \\r\\n>, "cols": N, "rows": N}.

For every screen this writes two fixtures into the output directory:

  NAME.txt          the text with SGR escapes removed: `rows` lines, each
                    exactly `cols` characters wide, with a trailing newline.
  NAME.colors.json  one entry per row. Each entry is a list of style runs
                    [col, fg, bg, bold]: col is the 0-based starting column of
                    a run of identical style, fg/bg are lowercase #rrggbb (or
                    null when unset), bold is a boolean. Adjacent runs with
                    identical style are merged.

Three committed fixtures carry one approved substitution that the mockup
HTML does not: the ash metadata grey #62666d is #787d85 in

  tests/fixtures/mockup/board.colors.json
  tests/fixtures/mockup/mac.colors.json
  tests/fixtures/mockup/modal.colors.json

(lifted for contrast, Dang, 2026-10-22). Re-running this script writes the
original #62666d back into them; swap it again in those three files, and only
those, before committing:

  cd tests/fixtures/mockup
  sed -i 's/#62666d/#787d85/g' board.colors.json mac.colors.json modal.colors.json

(on macOS: sed -i ''). tests/fixtures/mockup/cli.colors.json is not used by
the TUI tests and keeps #62666d.

Usage (Python standard library only):

  python3 -I docs/design/extract_mockup.py [HTML_PATH] [OUT_DIR]

Defaults are docs/design/tui-mockup.html and tests/fixtures/mockup, relative
to the repository root that contains this script.
"""

import json
import os
import re
import sys
import unicodedata

REPO_ROOT = os.path.dirname(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
)
DEFAULT_HTML = os.path.join(REPO_ROOT, "docs", "design", "tui-mockup.html")
DEFAULT_OUT = os.path.join(REPO_ROOT, "tests", "fixtures", "mockup")

ANCHOR = "const SCREENS = "

# One SGR sequence (ESC [ params m), a row break (CRLF), or a single character.
# Anything else (a bare ESC, a lone CR or LF, another CSI) is rejected by
# parse_screen, so the fixtures never silently drop a control sequence.
TOKEN_RE = re.compile(r"\x1b\[([0-9;]*)m|\r\n|([\s\S])")


def fail(message):
    raise SystemExit(f"extract_mockup: {message}")


def load_screens(html_path):
    with open(html_path, encoding="utf-8") as f:
        text = f.read()
    if text.count(ANCHOR) != 1:
        fail(f"expected exactly one '{ANCHOR}' in {html_path}")
    start = text.index(ANCHOR) + len(ANCHOR)
    # The object is JSON-compatible (\u001b and \r\n escapes), so decode it
    # directly from its opening brace and ignore the JavaScript after it.
    screens, _ = json.JSONDecoder().raw_decode(text, start)
    return screens


def apply_sgr(name, style, params):
    """Update the running style for one SGR sequence (terminal semantics:
    state persists until reset by code 0)."""
    codes = [int(p) if p else 0 for p in params.split(";")]
    i = 0
    while i < len(codes):
        code = codes[i]
        if code == 0:
            style.update(fg=None, bg=None, bold=False)
        elif code == 1:
            style["bold"] = True
        elif code == 22:
            style["bold"] = False
        elif code in (38, 48):
            if i + 4 >= len(codes) or codes[i + 1] != 2:
                fail(f"{name}: only 38;2;r;g;b and 48;2;r;g;b colours are supported (ESC[{params}m)")
            rgb = codes[i + 2:i + 5]
            if any(not 0 <= v <= 255 for v in rgb):
                fail(f"{name}: colour out of range in ESC[{params}m")
            style["fg" if code == 38 else "bg"] = "#%02x%02x%02x" % tuple(rgb)
            i += 5
            continue
        elif code == 39:
            style["fg"] = None
        elif code == 49:
            style["bg"] = None
        else:
            fail(f"{name}: unsupported SGR code {code} in ESC[{params}m")
        i += 1


def parse_screen(name, screen):
    """Return a list of rows; each row is a list of (char, fg, bg, bold) cells."""
    cols, expected_rows = screen["cols"], screen["rows"]
    style = {"fg": None, "bg": None, "bold": False}
    rows = [[]]
    for m in TOKEN_RE.finditer(screen["ansi"]):
        params, char = m.group(1), m.group(2)
        if params is not None:
            apply_sgr(name, style, params)
        elif char is None:
            rows.append([])
        else:
            if (unicodedata.category(char) == "Cc"
                    or unicodedata.east_asian_width(char) in ("W", "F")):
                fail(f"{name}: unexpected character {char!r} at row {len(rows) - 1}, "
                     f"col {len(rows[-1])}")
            rows[-1].append((char, style["fg"], style["bg"], style["bold"]))

    if len(rows) != expected_rows:
        fail(f"{name}: found {len(rows)} rows, screen says {expected_rows}")
    for number, row in enumerate(rows):
        if len(row) != cols:
            fail(f"{name}: row {number} is {len(row)} cells wide, screen says {cols}")
    return rows


def row_text(row):
    return "".join(char for char, _, _, _ in row)


def row_runs(row):
    runs = []
    for col, (_, fg, bg, bold) in enumerate(row):
        if runs and runs[-1][1:] == [fg, bg, bold]:
            continue
        runs.append([col, fg, bg, bold])
    return runs


def write_screen(out_dir, name, rows):
    text_path = os.path.join(out_dir, f"{name}.txt")
    with open(text_path, "w", encoding="utf-8", newline="\n") as f:
        f.write("".join(row_text(row) + "\n" for row in rows))

    # One row of runs per line keeps the colours fixture diffable.
    colors_path = os.path.join(out_dir, f"{name}.colors.json")
    entries = [json.dumps(row_runs(row), separators=(",", ":")) for row in rows]
    with open(colors_path, "w", encoding="utf-8", newline="\n") as f:
        f.write("[\n" + ",\n".join("  " + entry for entry in entries) + "\n]\n")


def main(argv):
    html_path = argv[1] if len(argv) > 1 else DEFAULT_HTML
    out_dir = argv[2] if len(argv) > 2 else DEFAULT_OUT
    screens = load_screens(html_path)
    os.makedirs(out_dir, exist_ok=True)
    for name, screen in screens.items():
        rows = parse_screen(name, screen)
        write_screen(out_dir, name, rows)
        print(f"{name}: {len(rows)} rows x {screen['cols']} cols")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
