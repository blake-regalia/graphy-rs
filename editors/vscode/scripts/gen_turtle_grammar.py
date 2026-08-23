#!/usr/bin/env python3
"""Generate syntaxes/turtle.tmLanguage.json — a TextMate port of the
Turtle-family grammar from blake-regalia/linked-data.syntaxes.

The upstream compiled .sublime-syntax is a ~72k-line generated context machine
(positional roles, expected-token error recovery) that TextMate cannot
express. This port keeps the lexical layer — every terminal of the Turtle/TriG
grammar with the upstream scope names (`.ttl` suffix) — so Macaron theme rules
bind, while diagnostics/roles stay with graphy-lsp.

Usage: python3 gen_turtle_grammar.py  (regenerates ../syntaxes/turtle.tmLanguage.json)
"""

import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "syntaxes", "turtle.tmLanguage.json")

# --- W3C Turtle terminals (regexes lifted from upstream syntax-source) ------

PN_CHARS_BASE = (
    r"A-Za-z"
    r"\x{00C0}-\x{00D6}\x{00D8}-\x{00F6}\x{00F8}-\x{02FF}"
    r"\x{0370}-\x{037D}\x{037F}-\x{1FFF}\x{200C}-\x{200D}"
    r"\x{2070}-\x{218F}\x{2C00}-\x{2FEF}\x{3001}-\x{D7FF}"
    r"\x{F900}-\x{FDCF}\x{FDF0}-\x{FFFD}\x{10000}-\x{EFFFF}"
)
PN_CHARS_U = PN_CHARS_BASE + r"_"
PN_CHARS = PN_CHARS_U + r"\-0-9\x{00B7}\x{0300}-\x{036F}\x{203F}-\x{2040}"

PN_PREFIX = rf"[{PN_CHARS_BASE}](?:[{PN_CHARS}.]*[{PN_CHARS}])?"
PLX = r"(?:%[0-9A-Fa-f]{2}|\\[-_~.!$&'()*+,;=/?#@%])"
PN_LOCAL = (
    rf"(?:[{PN_CHARS_U}:0-9]|{PLX})"
    rf"(?:(?:[{PN_CHARS}.:]|{PLX})*(?:[{PN_CHARS}:]|{PLX}))?"
)
BLANK_LABEL = rf"[{PN_CHARS_U}0-9](?:[{PN_CHARS}.]*[{PN_CHARS}])?"

UCHAR = r"\\u[0-9A-Fa-f]{4}|\\U[0-9A-Fa-f]{8}"
ECHAR_D = r"\\[tbnrf\"'\\]"

# --- repository ---------------------------------------------------------------


def string_rule(name, delim, long, quote_char):
    """A string literal context: escapes scoped, bad escapes flagged."""
    esc = [
        {
            "match": UCHAR,
            "name": f"constant.character.escape.literal.unicode.{name}.ttl",
        },
        {
            "match": ECHAR_D,
            "name": f"constant.character.escape.literal.escape.{name}.ttl",
        },
        {"match": r"\\.", "name": "invalid.illegal.escape.ttl"},
    ]
    rule = {
        "begin": delim,
        "end": delim if long else f"{delim}|$",
        "name": f"string.quoted.{quote_char}.literal.{name}.ttl",
        "beginCaptures": {
            "0": {"name": f"punctuation.definition.string.begin.literal.{quote_char}.{name}.ttl"}
        },
        "endCaptures": {
            "0": {"name": f"punctuation.definition.string.end.literal.{quote_char}.{name}.ttl"}
        },
        "patterns": esc,
    }
    return rule


REPOSITORY = {
    "comment": {
        "match": r"(#).*$",
        "name": "comment.line.ttl",
        "captures": {"1": {"name": "punctuation.definition.comment.ttl"}},
    },
    "directive-at": {
        "begin": r"(@)(prefix|base)\b",
        "end": r"\.",
        "name": "meta.directive.ttl",
        "beginCaptures": {
            "1": {"name": "punctuation.definition.storage.prefix.at.ttl"},
            "2": {"name": "storage.type.prefix.at.ttl"},
        },
        "endCaptures": {"0": {"name": "punctuation.terminator.prefix-declaration.ttl"}},
        "patterns": [
            {"include": "#comment"},
            {"include": "#iri"},
            {"include": "#prefixed-name"},
        ],
    },
    "directive-sparql": {
        "match": r"(?i)^\s*(prefix|base)\b",
        "captures": {"1": {"name": "storage.type.prefix.sparql.ttl"}},
    },
    "iri": {
        "begin": r"<",
        "end": r">|$",
        "name": "string.unquoted.iri.ttl",
        "beginCaptures": {"0": {"name": "punctuation.definition.iri.begin.ttl"}},
        "endCaptures": {"0": {"name": "punctuation.definition.iri.end.ttl"}},
        "patterns": [
            {"match": UCHAR, "name": "constant.character.escape.iri.ttl"},
            {"match": r"[<\"{}|^`\\ ]", "name": "invalid.illegal.iri.ttl"},
        ],
    },
    "prefixed-name": {
        "match": rf"((?:{PN_PREFIX})?)(:)((?:{PN_LOCAL})?)",
        "captures": {
            "1": {"name": "variable.other.readwrite.prefixed-name.namespace.ttl"},
            "2": {"name": "punctuation.separator.prefixed-name.namespace.ttl"},
            "3": {
                "name": "variable.other.member.prefixed-name.local.ttl",
                "patterns": [
                    {
                        "match": PLX,
                        "name": "constant.character.escape.prefixed-name.local.ttl",
                    }
                ],
            },
        },
    },
    "blank-node": {
        "match": rf"(_)(:)({BLANK_LABEL})",
        "captures": {
            "1": {"name": "variable.other.readwrite.blank-node.underscore.ttl"},
            "2": {"name": "punctuation.separator.prefixed-name.namespace.ttl"},
            "3": {"name": "variable.other.member.blank-node.label.ttl"},
        },
    },
    "string-long-double": string_rule("long", '"""', True, "double"),
    "string-long-single": string_rule("long", "'''", True, "single"),
    "string-short-double": string_rule("short", '"', False, "double"),
    "string-short-single": string_rule("short", "'", False, "single"),
    "language-tag": {
        "match": r"(@)([A-Za-z]+(?:-[A-Za-z0-9]+)*)",
        "captures": {
            "1": {"name": "punctuation.separator.language-tag.symbol.ttl"},
            "2": {"name": "string.unquoted.language-tag.ttl"},
        },
    },
    "datatype": {
        "begin": r"\^\^",
        "end": r"(?!\G)",
        "contentName": "meta.datatype.ttl",
        "beginCaptures": {"0": {"name": "punctuation.separator.datatype.symbol.ttl"}},
        "patterns": [{"include": "#iri"}, {"include": "#prefixed-name"}],
    },
    "number": {
        "patterns": [
            {
                "match": r"[+-]?(?:\d+\.\d*|\.?\d+)[eE][+-]?\d+",
                "name": "constant.numeric.double.ttl",
            },
            {
                "match": r"[+-]?(?:\d+\.\d*|\.\d+)",
                "name": "constant.numeric.decimal.ttl",
            },
            {"match": r"[+-]?\d+", "name": "constant.numeric.integer.ttl"},
        ]
    },
    "boolean": {
        "patterns": [
            {
                "match": r"\btrue\b",
                "name": "constant.language.boolean.true.ttl",
            },
            {
                "match": r"\bfalse\b",
                "name": "constant.language.boolean.false.ttl",
            },
        ]
    },
    "keyword-a": {
        "match": rf"(?<![\w:])a(?![{PN_CHARS}:])",
        "name": "support.constant.predicate.a.ttl",
    },
    "keyword-graph": {
        "match": r"(?i)\bGRAPH\b",
        "name": "keyword.control.graph.trig",
    },
    "triple-x": {
        "patterns": [
            {"match": r"<<", "name": "punctuation.definition.triple-x.begin.ttl"},
            {"match": r">>", "name": "punctuation.definition.triple-x.end.ttl"},
        ]
    },
    "punctuation": {
        "patterns": [
            {"match": r";", "name": "punctuation.separator.predicate-object-list.ttl"},
            {"match": r",", "name": "punctuation.separator.object.ttl"},
            {"match": r"\.", "name": "punctuation.terminator.triple.ttl"},
            {"match": r"\[", "name": "punctuation.definition.blank-node-property-list.begin.ttl"},
            {"match": r"\]", "name": "punctuation.definition.blank-node-property-list.end.ttl"},
            {"match": r"\(", "name": "punctuation.section.collection.begin.ttl"},
            {"match": r"\)", "name": "punctuation.section.collection.end.ttl"},
            {"match": r"\{", "name": "punctuation.section.graph.begin.trig"},
            {"match": r"\}", "name": "punctuation.section.graph.end.trig"},
        ]
    },
}

MAIN = [
    {"include": "#comment"},
    {"include": "#directive-at"},
    {"include": "#directive-sparql"},
    {"include": "#string-long-double"},
    {"include": "#string-long-single"},
    {"include": "#string-short-double"},
    {"include": "#string-short-single"},
    {"include": "#triple-x"},
    {"include": "#iri"},
    {"include": "#blank-node"},
    {"include": "#datatype"},
    {"include": "#language-tag"},
    {"include": "#keyword-a"},
    {"include": "#keyword-graph"},
    {"include": "#boolean"},
    {"include": "#prefixed-name"},
    {"include": "#number"},
    {"include": "#punctuation"},
]

GRAMMAR = {
    "$schema": "https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json",
    "name": "Turtle",
    "scopeName": "source.ttl",
    "patterns": MAIN,
    "repository": REPOSITORY,
}

if __name__ == "__main__":
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(GRAMMAR, f, indent="\t")
        f.write("\n")
    print(f"wrote {os.path.normpath(OUT)}")
