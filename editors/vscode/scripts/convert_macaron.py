#!/usr/bin/env python3
"""Convert the compiled Macaron sublime-color-schemes (from
blake-regalia/linked-data.syntaxes, built via `npm run build`) into VS Code
color themes.

- `globals` map to workbench editor colors.
- `rules` map to `tokenColors` (VS Code supports foreground + fontStyle only;
  background/foreground_adjust rules are dropped).
- `semanticTokenColors` are derived from the same palette by looking up the
  scheme rule for each graphy-lsp semantic token type, so the server's
  semantic tokens get Macaron colors even before any TextMate grammar exists.

Usage: python3 convert_macaron.py  (regenerates ../themes/*.json)
"""

import colorsys
import json
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))
UPSTREAM = os.path.join(HERE, "upstream")
THEMES = os.path.join(HERE, "..", "themes")

HSL = re.compile(r"hsla?\(\s*([\d.]+),\s*([\d.]+)%,\s*([\d.]+)%(?:,\s*([\d.]+))?\s*\)")
VAR = re.compile(r"var\((\w+)\)")


def to_hex(color, variables):
    """Normalize a scheme color (hsl/hsla/var/#hex) to #RRGGBB[AA]."""
    if not color:
        return None
    m = VAR.fullmatch(color.strip())
    if m:
        color = variables.get(m.group(1), "")
    if color.startswith("#"):
        return color
    m = HSL.match(color)
    if not m:
        return None
    h, s, l = float(m.group(1)) / 360, float(m.group(2)) / 100, float(m.group(3)) / 100
    a = m.group(4)
    r, g, b = colorsys.hls_to_rgb(h % 1.0, l, s)
    out = "#%02x%02x%02x" % (round(r * 255), round(g * 255), round(b * 255))
    if a is not None:
        out += "%02x" % round(float(a) * 255)
    return out


def font_style(rule):
    fs = rule.get("font_style", "")
    styles = [w for w in fs.split() if w in ("italic", "bold", "underline")]
    return " ".join(styles) if styles else None


# graphy-lsp semantic token type -> representative Macaron scope. Colors are
# looked up from the scheme's own rules so dark/light both derive correctly.
SEMANTIC_MAP = {
    "namespace": "variable.other.readwrite.prefixed-name.namespace",
    "type": "storage.type",
    "class": "string.unquoted.iri",
    "enumMember": "support.constant",
    "variable": "variable.other.readwrite.var",
    "property": "variable.other.member.prefixed-name.local",
    "string": "string.quoted.double.literal, string.quoted.single.literal",
    "number": "constant.numeric",
    "keyword": "keyword.control",
    "operator": "keyword.operator",
    "comment": "comment",
    "macro": "meta.directive",
    "decorator": "string.unquoted.language-tag",
}

# Sublime global -> VS Code workbench color key(s).
GLOBALS_MAP = {
    "foreground": ["editor.foreground", "foreground"],
    "background": ["editor.background"],
    "caret": ["editorCursor.foreground"],
    "selection": ["editor.selectionBackground"],
    "line_highlight": ["editor.lineHighlightBackground"],
    "gutter": ["editorGutter.background"],
    "guide": ["editorIndentGuide.background"],
    "active_guide": ["editorIndentGuide.activeBackground"],
    "find_highlight": ["editor.findMatchBackground"],
    "multi_edit_highlight": ["editor.selectionHighlightBackground"],
    "accent": ["focusBorder"],
}


def convert(src_name, out_name, label, ui_theme):
    with open(os.path.join(UPSTREAM, src_name)) as f:
        scheme = json.load(f)
    variables = scheme.get("variables", {})

    colors = {}
    for key, value in scheme.get("globals", {}).items():
        hexval = to_hex(value, variables)
        if hexval:
            for vs_key in GLOBALS_MAP.get(key, []):
                colors[vs_key] = hexval

    token_colors = []
    rule_index = {}
    for rule in scheme.get("rules", []):
        rule_index.setdefault(rule["scope"], rule)
        settings = {}
        fg = to_hex(rule.get("foreground"), variables)
        if fg:
            settings["foreground"] = fg
        fs = font_style(rule)
        if fs:
            settings["fontStyle"] = fs
        if not settings:
            continue  # background-only rule: not expressible in VS Code
        token_colors.append({"scope": rule["scope"], "settings": settings})

    semantic = {}
    for token_type, scope in SEMANTIC_MAP.items():
        rule = rule_index.get(scope)
        if not rule:
            continue
        entry = {}
        fg = to_hex(rule.get("foreground"), variables)
        if fg:
            entry["foreground"] = fg
        fs = font_style(rule)
        if fs:
            entry["fontStyle"] = fs
        if entry:
            semantic[token_type] = entry

    theme = {
        "$schema": "vscode://schemas/color-theme",
        "name": label,
        "type": ui_theme,
        "semanticHighlighting": True,
        "colors": colors,
        "semanticTokenColors": semantic,
        "tokenColors": token_colors,
    }
    os.makedirs(THEMES, exist_ok=True)
    out_path = os.path.join(THEMES, out_name)
    with open(out_path, "w") as f:
        json.dump(theme, f, indent="\t")
        f.write("\n")
    print(f"wrote {out_path}: {len(token_colors)} tokenColors, "
          f"{len(semantic)} semanticTokenColors, {len(colors)} workbench colors")


if __name__ == "__main__":
    convert(
        "macaron-dark.sublime-color-scheme",
        "macaron-dark-color-theme.json",
        "Macaron Dark (Graphy)",
        "dark",
    )
    convert(
        "macaron-light.sublime-color-scheme",
        "macaron-light-color-theme.json",
        "Macaron Light (Graphy)",
        "light",
    )
