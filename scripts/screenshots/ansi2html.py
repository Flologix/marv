#!/usr/bin/env python3
"""Turns `tmux capture-pane -e -p` output (ANSI SGR colors) into a standalone HTML page that looks
like a terminal window, for headless Chromium to screenshot.

    python3 ansi2html.py capture.ans out.html [title]
"""

import html
import re
import subprocess
import sys

BG, FG = "#1c1c1f", "#d4d4d8"
# The 16 base colors (xterm-ish, tuned for the dark background).
BASE = ["#27272a", "#f87171", "#4ade80", "#facc15", "#60a5fa", "#c084fc", "#2dd4bf", "#d4d4d8",
        "#71717a", "#fca5a5", "#86efac", "#fde047", "#93c5fd", "#d8b4fe", "#5eead4", "#fafafa"]


# Box-drawing characters the TUI uses, drawn as CSS lines (fonts leave gaps between rows).
BOX = {"│": "v", "─": "h", "╭": "tl", "╮": "tr", "╰": "bl", "╯": "br", "⎿": "el"}


def color256(n):
    if n < 16:
        return BASE[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (steps[n // 36], steps[n // 6 % 6], steps[n % 6])
    v = 8 + 10 * (n - 232)
    return "#%02x%02x%02x" % (v, v, v)


def apply(params, st):
    p = [int(x) if x else 0 for x in params.split(";")] if params else [0]
    i = 0
    while i < len(p):
        c = p[i]
        if c == 0:
            st.clear()
        elif c in (1, 2, 3, 4, 7):
            st[{1: "b", 2: "dim", 3: "i", 4: "u", 7: "rev"}[c]] = True
        elif c == 22:
            st.pop("b", None), st.pop("dim", None)
        elif c in (23, 24, 27):
            st.pop({23: "i", 24: "u", 27: "rev"}[c], None)
        elif 30 <= c <= 37 or 90 <= c <= 97:
            st["fg"] = BASE[c - 30 if c < 90 else c - 82]
        elif 40 <= c <= 47 or 100 <= c <= 107:
            st["bg"] = BASE[c - 40 if c < 100 else c - 92]
        elif c in (38, 48):
            key = "fg" if c == 38 else "bg"
            if p[i + 1] == 5:
                st[key] = color256(p[i + 2])
                i += 2
            elif p[i + 1] == 2:
                st[key] = "#%02x%02x%02x" % tuple(p[i + 2:i + 5])
                i += 4
        elif c == 39:
            st.pop("fg", None)
        elif c == 49:
            st.pop("bg", None)
        i += 1


def style(st):
    fg, bg = st.get("fg", FG), st.get("bg")
    if st.get("rev"):
        fg, bg = bg or BG, fg
    css = [f"color:{fg}"]
    if bg:
        css.append(f"background:{bg}")
    if st.get("b"):
        css.append("font-weight:700")
    if st.get("dim"):
        css.append("opacity:.6")
    if st.get("i"):
        css.append("font-style:italic")
    if st.get("u"):
        css.append("text-decoration:underline")
    return ";".join(css)


def convert(text):
    st, out = {}, []
    for line in text.rstrip("\n").split("\n"):
        row = []
        for part in re.split(r"(\x1b\[[0-9;]*m)", line):
            if part.startswith("\x1b["):
                apply(part[2:-1], st)
            elif part:
                text = html.escape(part)
                text = re.sub(f"[{''.join(BOX)}]", lambda m: f'<i class="bx {BOX[m.group()]}"></i>', text)
                row.append(f'<span style="{style(st)}">{text}</span>')
        out.append("".join(row))
    return out


def font_file(style):
    """A JetBrains Mono file of this style, if fontconfig knows one ("" otherwise: plain monospace)."""
    try:
        listing = subprocess.run(["fc-list", ":", "file", "family", "style"],
                                 capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return ""
    for line in sorted(listing.splitlines()):
        path, _, rest = line.partition(":")
        if "JetBrains" in rest and "Mono" in rest and re.search(rf"style={style}\b", rest):
            return path
    return ""


def main():
    src, dst = sys.argv[1], sys.argv[2]
    title = sys.argv[3] if len(sys.argv) > 3 else "marv"
    rows = convert(open(src, encoding="utf-8").read())
    body = "".join(f'<div class="r">{r or " "}</div>' for r in rows)
    regular, bold = font_file("Regular"), font_file("Bold")
    page = f"""<!doctype html><meta charset="utf-8"><style>
@font-face {{ font-family: Term; src: url("file://{regular}"); }}
@font-face {{ font-family: Term; font-weight: 700; src: url("file://{bold}"); }}
html, body {{ margin: 0; background: #0f0f11; }}
.win {{ margin: 0; background: {BG}; border-radius: 0; overflow: hidden; }}
.bar {{ height: 30px; display: flex; align-items: center; gap: 8px; padding: 0 14px; background: #27272a;
  font: 13px system-ui, sans-serif; color: #a1a1aa; }}
.dot {{ width: 12px; height: 12px; border-radius: 50%; }}
.bar span.t {{ flex: 1; text-align: center; margin-right: 60px; }}
pre {{ margin: 0; padding: 10px 14px 12px; font: 15px/20px Term, "DejaVu Sans Mono", monospace; font-variant-ligatures: none; font-feature-settings: "calt" 0, "liga" 0; color: {FG}; }}
.r {{ height: 20px; white-space: pre; }}
.bx {{ display: inline-block; position: relative; width: 1ch; height: 20px; vertical-align: top; }}
.bx::before {{ content: ""; position: absolute; border: 0 solid currentColor; }}
.v::before {{ left: 50%; top: 0; bottom: 0; border-left-width: 1.5px; }}
.h::before {{ top: 50%; left: 0; right: 0; border-top-width: 1.5px; }}
.tl::before {{ left: 50%; top: 50%; right: 0; bottom: 0; border-left-width: 1.5px; border-top-width: 1.5px; border-top-left-radius: 6px; }}
.tr::before {{ left: 0; top: 50%; right: calc(50% - 1.5px); bottom: 0; border-right-width: 1.5px; border-top-width: 1.5px; border-top-right-radius: 6px; }}
.bl::before {{ left: 50%; top: 0; right: 0; bottom: calc(50% - 1.5px); border-left-width: 1.5px; border-bottom-width: 1.5px; border-bottom-left-radius: 6px; }}
.br::before {{ left: 0; top: 0; right: calc(50% - 1.5px); bottom: calc(50% - 1.5px); border-right-width: 1.5px; border-bottom-width: 1.5px; border-bottom-right-radius: 6px; }}
.el::before {{ left: 50%; top: 0; right: 0; bottom: calc(50% - 1.5px); border-left-width: 1.5px; border-bottom-width: 1.5px; }}
</style><div class="win"><div class="bar"><i class="dot" style="background:#f87171"></i>
<i class="dot" style="background:#facc15"></i><i class="dot" style="background:#4ade80"></i>
<span class="t">{html.escape(title)}</span></div><pre>{body}</pre></div>"""
    open(dst, "w", encoding="utf-8").write(page)


if __name__ == "__main__":
    main()
