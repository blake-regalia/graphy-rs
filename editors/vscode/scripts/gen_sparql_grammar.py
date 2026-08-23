#!/usr/bin/env python3
"""Generate syntaxes/sparql.tmLanguage.json — a TextMate port of the SPARQL
grammar from blake-regalia/linked-data.syntaxes (lexical layer, `.rq` scope
suffix; see gen_turtle_grammar.py for the porting rationale).

Usage: python3 gen_sparql_grammar.py  (regenerates ../syntaxes/sparql.tmLanguage.json)
"""

import json
import os

from gen_turtle_grammar import (
    BLANK_LABEL,
    ECHAR_D,
    PLX,
    PN_CHARS,
    PN_LOCAL,
    PN_PREFIX,
    PN_CHARS_U,
    UCHAR,
)

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "syntaxes", "sparql.tmLanguage.json")

VARNAME = rf"[{PN_CHARS_U}0-9][{PN_CHARS_U}0-9\x{{00B7}}\x{{0300}}-\x{{036F}}\x{{203F}}-\x{{2040}}]*"
IRI = r"<[^\s<>\"{}|^`\\]*>"
KW_END = rf"(?![{PN_CHARS}:])"

QUALIFIERS = (
    "select|construct|describe|ask|where|values|insert|delete|load|clear|"
    "drop|create|add|move|copy|data"
)
MODIFIERS = (
    "as|bind|by|from|graph|having|into|limit|named|offset|order|silent|"
    "with|using|to|service|in|not|separator|filter|exists"
)
BUILTINS = (
    "str|lang|langmatches|datatype|bound|iri|uri|bnode|rand|abs|ceil|floor|"
    "round|concat|strlen|ucase|lcase|encode_for_uri|contains|strstarts|"
    "strends|strbefore|strafter|year|month|day|hours|minutes|seconds|"
    "timezone|tz|now|uuid|struuid|md5|sha1|sha256|sha384|sha512|coalesce|"
    "if|strlang|strdt|sameterm|isiri|isuri|isblank|isliteral|isnumeric|"
    "regex|substr|replace"
)
AGGREGATES = "count|sum|min|max|avg|sample|group_concat"


def string_rule(name, delim, long, quote_char):
    return {
        "begin": delim,
        "end": delim if long else f"{delim}|$",
        "name": f"string.quoted.{quote_char}.literal.{name}.rq",
        "beginCaptures": {
            "0": {"name": f"punctuation.definition.string.begin.literal.{quote_char}.{name}.rq"}
        },
        "endCaptures": {
            "0": {"name": f"punctuation.definition.string.end.literal.{quote_char}.{name}.rq"}
        },
        "patterns": [
            {
                "match": UCHAR,
                "name": f"constant.character.escape.literal.unicode.{name}.rq",
            },
            {
                "match": ECHAR_D,
                "name": f"constant.character.escape.literal.escape.{name}.rq",
            },
            {"match": r"\\.", "name": "invalid.illegal.escape.rq"},
        ],
    }


REPOSITORY = {
    "comment": {
        "match": r"(#).*$",
        "name": "comment.line.rq",
        "captures": {"1": {"name": "punctuation.definition.comment.rq"}},
    },
    "iri": {
        "match": rf"(<)([^<>]*)(>)",
        "name": "meta.iri.rq",
        "captures": {
            "1": {"name": "punctuation.definition.iri.begin.rq"},
            "2": {
                "name": "string.unquoted.iri.rq",
                "patterns": [
                    {"match": UCHAR, "name": "constant.character.escape.iri.rq"}
                ],
            },
            "3": {"name": "punctuation.definition.iri.end.rq"},
        },
    },
    "var": {
        "patterns": [
            {
                "match": rf"(\?)(?:{VARNAME})",
                "name": "variable.other.readwrite.var.question-mark.rq",
                "captures": {
                    "1": {"name": "punctuation.definition.variable.var.question-mark.rq"}
                },
            },
            {
                "match": rf"(\$)(?:{VARNAME})",
                "name": "variable.other.readwrite.var.dollar-sign.rq",
                "captures": {
                    "1": {"name": "punctuation.definition.variable.var.dollar-sign.rq"}
                },
            },
        ]
    },
    "prefixed-name": {
        "match": rf"((?:{PN_PREFIX})?)(:)((?:{PN_LOCAL})?)",
        "captures": {
            "1": {"name": "variable.other.readwrite.prefixed-name.namespace.rq"},
            "2": {"name": "punctuation.separator.prefixed-name.namespace.rq"},
            "3": {
                "name": "variable.other.member.prefixed-name.local.rq",
                "patterns": [
                    {
                        "match": PLX,
                        "name": "constant.character.escape.prefixed-name.local.rq",
                    }
                ],
            },
        },
    },
    "blank-node": {
        "match": rf"(_)(:)({BLANK_LABEL})",
        "captures": {
            "1": {"name": "variable.other.readwrite.blank-node.underscore.rq"},
            "2": {"name": "punctuation.separator.prefixed-name.namespace.rq"},
            "3": {"name": "variable.other.member.blank-node.label.rq"},
        },
    },
    "string-long-double": string_rule("long", '"""', True, "double"),
    "string-long-single": string_rule("long", "'''", True, "single"),
    "string-short-double": string_rule("short", '"', False, "double"),
    "string-short-single": string_rule("short", "'", False, "single"),
    "language-tag": {
        "match": r"(@)([A-Za-z]+(?:-[A-Za-z0-9]+)*)",
        "captures": {
            "1": {"name": "punctuation.separator.language-tag.symbol.rq"},
            "2": {"name": "string.unquoted.language-tag.rq"},
        },
    },
    "datatype": {
        "begin": r"\^\^",
        "end": r"(?!\G)",
        "contentName": "meta.datatype.rq",
        "beginCaptures": {"0": {"name": "punctuation.separator.datatype.symbol.rq"}},
        "patterns": [{"include": "#iri"}, {"include": "#prefixed-name"}],
    },
    "keywords": {
        "patterns": [
            {
                "match": rf"(?i)\b(?:prefix){KW_END}",
                "name": "storage.type.prefix.sparql.rq",
            },
            {
                "match": rf"(?i)\b(?:base){KW_END}",
                "name": "storage.type.base.sparql.rq",
            },
            {
                "match": rf"(?i)\b(?:distinct){KW_END}",
                "name": "storage.modifier.distinct.rq",
            },
            {
                "match": rf"(?i)\b(?:reduced){KW_END}",
                "name": "storage.modifier.reduced.rq",
            },
            {
                "match": rf"(?i)\b(?:optional){KW_END}",
                "name": "keyword.operator.word.optional.rq",
            },
            {
                "match": rf"(?i)\b(?:union){KW_END}",
                "name": "keyword.operator.word.union.rq",
            },
            {
                "match": rf"(?i)\b(?:minus){KW_END}",
                "name": "keyword.operator.word.minus.rq",
            },
            {
                "match": rf"(?i)\b(?:group){KW_END}",
                "name": "keyword.operator.word.group.rq",
            },
            {
                "match": rf"(?i)\b(?:{QUALIFIERS}){KW_END}",
                "name": "keyword.operator.word.qualifier.rq",
            },
            {
                "match": rf"(?i)\b(?:{MODIFIERS}){KW_END}",
                "name": "keyword.operator.word.modifier.rq",
            },
            {
                "match": rf"(?i)\b(?:{AGGREGATES}){KW_END}",
                "name": "support.function.built-in.aggregate.rq",
            },
            {
                "match": rf"(?i)\b(?:{BUILTINS}){KW_END}",
                "name": "support.function.built-in.rq",
            },
            {
                "match": rf"(?i)\b(?:asc|desc){KW_END}",
                "name": "support.function.built-in.sort.rq",
            },
            {
                "match": rf"(?i)\b(?:undef){KW_END}",
                "name": "support.constant.undef.rq",
            },
            {
                "match": rf"(?i)\b(?:default|all){KW_END}",
                "name": "support.constant.graph.rq",
            },
            {
                "match": rf"(?i)\b(?:true){KW_END}",
                "name": "constant.language.boolean.true.rq",
            },
            {
                "match": rf"(?i)\b(?:false){KW_END}",
                "name": "constant.language.boolean.false.rq",
            },
            {
                "match": rf"(?<![\w:])a(?![{PN_CHARS}:])",
                "name": "support.constant.predicate.a.rq",
            },
        ]
    },
    "number": {
        "patterns": [
            {
                "match": r"[+-]?(?:\d+\.\d*|\.?\d+)[eE][+-]?\d+",
                "name": "constant.numeric.double.rq",
            },
            {
                "match": r"[+-]?(?:\d+\.\d*|\.\d+)",
                "name": "constant.numeric.decimal.rq",
            },
            {"match": r"[+-]?\d+", "name": "constant.numeric.integer.rq"},
        ]
    },
    "operators": {
        "patterns": [
            {"match": r"&&", "name": "keyword.operator.conditional.and.rq"},
            {"match": r"\|\|", "name": "keyword.operator.conditional.or.rq"},
            {"match": r"!=", "name": "keyword.operator.relational.non-equality.rq"},
            {"match": r"<=", "name": "keyword.operator.relational.less-than-or-equal-to.rq"},
            {"match": r">=", "name": "keyword.operator.relational.greater-than-or-equal-to.rq"},
            {"match": r"=", "name": "keyword.operator.relational.equality.rq"},
            {"match": r"<", "name": "keyword.operator.relational.less-than.rq"},
            {"match": r">", "name": "keyword.operator.relational.greater-than.rq"},
            {"match": r"!", "name": "keyword.operator.logical.not.rq"},
            {"match": r"\^", "name": "keyword.operator.path.inverse.rq"},
            {"match": r"\|", "name": "keyword.operator.path.alternative.rq"},
            {"match": r"/", "name": "keyword.operator.path.separator.rq"},
            {"match": r"\*", "name": "keyword.operator.star.rq"},
            {"match": r"\+", "name": "keyword.operator.arithmetic.addition.rq"},
            {"match": r"-", "name": "keyword.operator.arithmetic.subtraction.rq"},
            {"match": r"\?", "name": "keyword.operator.path.quantifier.zero-or-one.rq"},
        ]
    },
    "triple-x": {
        "patterns": [
            {"match": r"<<", "name": "punctuation.definition.triple-x.begin.rq"},
            {"match": r">>", "name": "punctuation.definition.triple-x.end.rq"},
        ]
    },
    "punctuation": {
        "patterns": [
            {"match": r"\{", "name": "punctuation.section.group.begin.rq"},
            {"match": r"\}", "name": "punctuation.section.group.end.rq"},
            {"match": r"\(", "name": "punctuation.definition.expression.begin.rq"},
            {"match": r"\)", "name": "punctuation.definition.expression.end.rq"},
            {"match": r"\[", "name": "punctuation.definition.blank-node-property-list.begin.rq"},
            {"match": r"\]", "name": "punctuation.definition.blank-node-property-list.end.rq"},
            {"match": r";", "name": "punctuation.separator.predicate-object-list.rq"},
            {"match": r",", "name": "punctuation.separator.object.rq"},
            {"match": r"\.", "name": "punctuation.terminator.triple.rq"},
        ]
    },
}

MAIN = [
    {"include": "#comment"},
    {"include": "#string-long-double"},
    {"include": "#string-long-single"},
    {"include": "#string-short-double"},
    {"include": "#string-short-single"},
    {"include": "#triple-x"},
    {"include": "#iri"},
    {"include": "#var"},
    {"include": "#blank-node"},
    {"include": "#datatype"},
    {"include": "#language-tag"},
    {"include": "#prefixed-name"},
    {"include": "#keywords"},
    {"include": "#number"},
    {"include": "#operators"},
    {"include": "#punctuation"},
]

GRAMMAR = {
    "$schema": "https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json",
    "name": "SPARQL",
    "scopeName": "source.rq",
    "patterns": MAIN,
    "repository": REPOSITORY,
}

if __name__ == "__main__":
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(GRAMMAR, f, indent="\t")
        f.write("\n")
    print(f"wrote {os.path.normpath(OUT)}")
