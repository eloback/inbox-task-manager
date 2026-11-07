"""Turns a `tmux capture-pane -e -p` capture into an HTML page that looks
like a terminal window, for scripts/screenshots.sh to photograph.

Usage: python3 -I scripts/ansi-to-html.py <capture> <columns> <background> > page.html

Reads SGR only (what capture-pane writes): bold, dim, italic, underline,
reverse, and 16, 256 and 24-bit colors. Attributes carry over from one
line to the next, as capture-pane leaves out what does not change.
"""

import html
import re
import sys

capture, columns, background = sys.argv[1], int(sys.argv[2]), sys.argv[3]
# The cells left at the default colors: the terminal's (a Catppuccin Mocha one).
DEFAULT_FG, DEFAULT_BG = "#cdd6f4", background
ANSI = ["#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#cba6f7", "#94e2d5", "#bac2de",
        "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#cba6f7", "#94e2d5", "#a6adc8"]


def xterm(n):
    if n < 16:
        return ANSI[n]
    if n >= 232:
        v = 8 + 10 * (n - 232)
        return f"#{v:02x}{v:02x}{v:02x}"
    n -= 16
    levels = [0, 95, 135, 175, 215, 255]
    return "#" + "".join(f"{levels[c]:02x}" for c in (n // 36, n // 6 % 6, n % 6))


def apply(state, params):
    codes = [int(p) if p else 0 for p in params.split(";")] if params else [0]
    i = 0
    while i < len(codes):
        c = codes[i]
        if c == 0:
            state.clear()
        elif c in (1, 2, 3, 4, 7):
            state[c] = True
        elif c == 22:
            state.pop(1, None), state.pop(2, None)
        elif c in (23, 24, 27):
            state.pop(c - 20, None)
        elif 30 <= c <= 37 or 90 <= c <= 97:
            state["fg"] = ANSI[c - 30 if c < 90 else c - 82]
        elif 40 <= c <= 47 or 100 <= c <= 107:
            state["bg"] = ANSI[c - 40 if c < 100 else c - 92]
        elif c in (39, 49):
            state.pop("fg" if c == 39 else "bg", None)
        elif c in (38, 48) and i + 1 < len(codes):
            key = "fg" if c == 38 else "bg"
            if codes[i + 1] == 5:
                state[key] = xterm(codes[i + 2])
                i += 2
            elif codes[i + 1] == 2:
                state[key] = "#{:02x}{:02x}{:02x}".format(*codes[i + 2:i + 5])
                i += 4
        i += 1


def style(state):
    fg, bg = state.get("fg", DEFAULT_FG), state.get("bg", DEFAULT_BG)
    if state.get(7):
        fg, bg = bg, fg
    css = f"color:{fg};background:{bg};"
    if state.get(1):
        css += "font-weight:700;"
    if state.get(2):
        css += "opacity:.6;"
    if state.get(3):
        css += "font-style:italic;"
    if state.get(4):
        css += "text-decoration:underline;"
    return css


rows, state = [], {}
for line in open(capture, encoding="utf-8").read().split("\n"):
    out, width = "", 0
    for part in re.split(r"(\x1b\[[0-9;]*m)", line):
        if part.startswith("\x1b["):
            apply(state, part[2:-1])
        elif part:
            width += len(part)
            out += f'<span style="{style(state)}">{html.escape(part)}</span>'
    out += f'<span style="background:{DEFAULT_BG}">{" " * max(0, columns - width)}</span>'
    rows.append(f'<div class="row">{out}</div>')
while len(rows) > 1 and rows[-1].endswith(f'{" " * columns}</span></div>'):
    rows.pop()

print(f"""<!doctype html><meta charset="utf-8"><style>
body {{ margin: 0; background: transparent; }}
.window {{ display: inline-block; margin: 24px; border-radius: 12px; overflow: hidden;
  background: {DEFAULT_BG}; box-shadow: 0 18px 50px rgba(0,0,0,.45), 0 0 0 1px rgba(255,255,255,.06); }}
.bar {{ height: 34px; display: flex; gap: 8px; align-items: center; padding-left: 14px; }}
.bar i {{ width: 12px; height: 12px; border-radius: 50%; display: block; }}
.screen {{ padding: 6px 18px 18px; font: 15px "FiraCode Nerd Font Mono", "JetBrains Mono", monospace; }}
.row {{ height: 20px; display: flex; }}
.row span {{ white-space: pre; line-height: 20px; height: 20px; }}
</style><div class="window"><div class="bar"><i style="background:#ff5f57"></i><i style="background:#febc2e"></i><i style="background:#28c840"></i></div>
<div class="screen">{"".join(rows)}</div></div>""")
