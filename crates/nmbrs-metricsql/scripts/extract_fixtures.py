#!/usr/bin/env python3
# Extract MetricsQL parser-test fixtures from upstream Go test
# files into JSON the Rust port consumes for parity testing.
#
# Usage:
#   python3 crates/nmbrs-metricsql/scripts/extract_fixtures.py
#
# The upstream test files use two helper closures:
#   same(s)            — parse s, re-print, expect identical s
#   another(s, want)   — parse s, re-print, expect want
#
# Output JSON shape (under crates/nmbrs-metricsql/tests/fixtures/):
#   { "source": "parser_test.go",
#     "round_trip": [ { "input": "...", "expected": "..." }, ... ] }

from __future__ import annotations
import json
import os
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UPSTREAM = (ROOT / ".." / "links" / "metricsql").resolve()
OUT_DIR = ROOT / "tests" / "fixtures"
OUT_DIR.mkdir(parents=True, exist_ok=True)


def parse_go_string(text: str, pos: int) -> tuple[str, int] | None:
    """Parse a Go string literal starting at text[pos].

    Returns (value, end_pos) on success, where end_pos is the
    index just past the closing delimiter. Returns None if
    text[pos] isn't the start of a string literal.

    Supports both interpreted strings (`"..."` with escape
    processing) and raw strings (`` `...` `` with no
    processing — backtick is not allowed inside).
    """
    if pos >= len(text):
        return None
    ch = text[pos]
    if ch == '`':
        end = text.find('`', pos + 1)
        if end == -1:
            return None
        return text[pos + 1 : end], end + 1
    if ch != '"':
        return None
    # Interpreted: walk char by char, processing escapes.
    out = []
    i = pos + 1
    while i < len(text):
        c = text[i]
        if c == '"':
            return ''.join(out), i + 1
        if c == '\\':
            if i + 1 >= len(text):
                return None
            nxt = text[i + 1]
            simple = {
                'n': '\n', 't': '\t', 'r': '\r', '0': '\0',
                'a': '\a', 'b': '\b', 'f': '\f', 'v': '\v',
                '\\': '\\', '"': '"', "'": "'", '`': '`',
            }
            if nxt in simple:
                out.append(simple[nxt])
                i += 2
                continue
            if nxt == 'x' and i + 3 < len(text):
                # \xHH — one byte
                hex_s = text[i + 2 : i + 4]
                try:
                    out.append(chr(int(hex_s, 16)))
                except ValueError:
                    return None
                i += 4
                continue
            if nxt == 'u' and i + 5 < len(text):
                # \uHHHH — BMP codepoint
                hex_s = text[i + 2 : i + 6]
                try:
                    out.append(chr(int(hex_s, 16)))
                except ValueError:
                    return None
                i += 6
                continue
            if nxt == 'U' and i + 9 < len(text):
                # \UHHHHHHHH — full codepoint
                hex_s = text[i + 2 : i + 10]
                try:
                    out.append(chr(int(hex_s, 16)))
                except ValueError:
                    return None
                i += 10
                continue
            # Octal \NNN: treat as literal backslash for now
            out.append(c)
            i += 1
            continue
        out.append(c)
        i += 1
    return None


def harvest_calls(go_source: str, name: str, arg_count: int):
    """Yield tuples of `arg_count` strings for every `name(...)`
    call in `go_source` whose arguments are all string literals."""
    # Match the function name as a word boundary, optionally
    # preceded by whitespace + an opening line. We deliberately
    # use a simple regex to find candidate call sites; the
    # heavy lifting is in `parse_go_string`.
    pattern = re.compile(r'\b' + re.escape(name) + r'\s*\(')
    for m in pattern.finditer(go_source):
        i = m.end()
        args: list[str] = []
        ok = True
        for arg_idx in range(arg_count):
            # Skip whitespace + commas
            while i < len(go_source) and go_source[i] in ' \t\n\r':
                i += 1
            parsed = parse_go_string(go_source, i)
            if parsed is None:
                ok = False
                break
            value, i = parsed
            args.append(value)
            # Expect comma between args, ) after last
            while i < len(go_source) and go_source[i] in ' \t\n\r':
                i += 1
            if arg_idx + 1 < arg_count:
                if i < len(go_source) and go_source[i] == ',':
                    i += 1
                else:
                    ok = False
                    break
            else:
                # Last arg — accept `)` or `, ...) `
                # (some callers use trailing comma).
                if i < len(go_source) and go_source[i] == ',':
                    i += 1
                    while i < len(go_source) and go_source[i] in ' \t\n\r':
                        i += 1
                if i < len(go_source) and go_source[i] == ')':
                    pass
                else:
                    ok = False
                    break
        if ok:
            yield tuple(args)


def harvest_round_trip(path: Path) -> list[dict]:
    """Pull `same(s)` and `another(s, expected)` from a Go test file."""
    src = path.read_text(encoding='utf-8')
    cases: list[dict] = []
    for (s,) in harvest_calls(src, 'same', 1):
        cases.append({'input': s, 'expected': s})
    for (s, expected) in harvest_calls(src, 'another', 2):
        cases.append({'input': s, 'expected': expected})
    return cases


def main():
    targets = [
        ('parser_test.go', 'parser_round_trip.json'),
        ('prettifier_test.go', 'prettifier_round_trip.json'),
    ]
    if not UPSTREAM.exists():
        print(f'ERROR: upstream metricsql not found at {UPSTREAM}', file=sys.stderr)
        sys.exit(1)

    total = 0
    for src_name, out_name in targets:
        src = UPSTREAM / src_name
        if not src.exists():
            print(f'skip {src_name}: not present')
            continue
        cases = harvest_round_trip(src)
        out = OUT_DIR / out_name
        out.write_text(
            json.dumps(
                {
                    'source': src_name,
                    'helper': 'same_or_another',
                    'round_trip': cases,
                },
                indent=2,
                ensure_ascii=False,
            ) + '\n',
            encoding='utf-8',
        )
        print(f'{out.name}: {len(cases)} cases')
        total += len(cases)

    print(f'total: {total} round-trip cases')


if __name__ == '__main__':
    main()
