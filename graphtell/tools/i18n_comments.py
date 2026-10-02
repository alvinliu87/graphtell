#!/usr/bin/env python3
"""Helper for translating source comments to English.

Two sub-commands:

    extract <out.json> <files...>
        Collect every comment block containing CJK text into `<out.json>` as a
        `{id: original}` map. Ids look like `path:12-30`.

    apply <originals.json> <translations.json>
        Replace each block with its English counterpart. Blocks are matched by
        exact text (indentation and comment markers included), so a translation
        can never land on the wrong line.

Only comments are touched. Recognised markers: `//!`, `///`, `//`, `/* */`,
JSX `{/* */}` for Rust / TS / JS; `#` for Python and YAML.
"""
from __future__ import annotations

import json
import os
import re
import sys

CJK = re.compile(r'[㐀-鿿豈-﫿]')
SLASH_EXT = {'.rs', '.ts', '.tsx', '.js', '.jsx'}
HASH_EXT = {'.py', '.yaml', '.yml'}


def classify(line: str, ext: str, in_block: bool):
    """Return (is_comment, still_in_block)."""
    s = line.strip()
    if ext in SLASH_EXT:
        if in_block:
            return True, ('*/' not in s)
        if s.startswith('//'):
            return True, False
        if s.startswith('/*') or s.startswith('{/*'):
            return True, ('*/' not in s)
        # A bare `*` only continues a block comment; `*ptr` / `*out =` is code.
        if s.startswith('*/}'):
            return True, False
        if s.startswith('* ') or s == '*':
            return True, False
        return False, False
    if ext in HASH_EXT:
        return s.startswith('#'), False
    return False, False


def blocks_of(path: str):
    ext = os.path.splitext(path)[1]
    lines = open(path, encoding='utf-8').read().split('\n')
    res = []
    i, n = 0, len(lines)
    in_block = False
    while i < n:
        is_c, in_block = classify(lines[i], ext, in_block)
        if is_c:
            j = i
            nb = in_block
            while j + 1 < n:
                c2, nb2 = classify(lines[j + 1], ext, nb)
                if not c2:
                    break
                nb = nb2
                j += 1
            in_block = nb
            if any(CJK.search(x) for x in lines[i:j + 1]):
                # trim trailing CJK-free lines
                while j > i and not CJK.search(lines[j]):
                    j -= 1
                res.append((i, j))
            i = j + 1
        else:
            i += 1
    return res


def is_doc_block(text: str) -> bool:
    """A doc block starts with `///` or `//!` (Rust) — those are kept by `prune`."""
    for line in text.split('\n'):
        s = line.strip()
        if s:
            return s.startswith('///') or s.startswith('//!')
    return False


def prune(paths, inline_min_lines, keep_doc):
    """Delete verbose comment blocks in place.

    Removes inline (`//` / `#` / `/* */`) comment blocks of `inline_min_lines` or more,
    which are narration ("why we used to do X") rather than load-bearing information.
    Doc comments (`///` / `//!`) are kept unless `keep_doc` is False.
    """
    removed = kept = 0
    for path in paths:
        lines = open(path, encoding='utf-8').read().split('\n')
        # recompute blocks against the original file, then filter
        blocks = blocks_of(path)
        drop = set()
        for a, b in blocks:
            text = '\n'.join(lines[a:b + 1])
            n = b - a + 1
            if keep_doc and is_doc_block(text):
                kept += 1
                continue
            if n >= inline_min_lines:
                drop.update(range(a, b + 1))
                removed += 1
            else:
                kept += 1
        out = [l for idx, l in enumerate(lines) if idx not in drop]
        open(path, 'w', encoding='utf-8').write('\n'.join(out))
    print('prune: removed %d block(s), kept %d' % (removed, kept))


def main() -> None:
    cmd = sys.argv[1]
    if cmd == 'prune':
        # usage: prune <inline_min_lines> <files...>
        n = int(sys.argv[2])
        prune(list(sys.argv[3:]), n, True)
        return
    if cmd == 'extract':
        out = sys.argv[2]
        data = {}
        for path in sys.argv[3:]:
            for a, b in blocks_of(path):
                text = '\n'.join(open(path, encoding='utf-8').read().split('\n')[a:b + 1])
                data['%s:%d-%d' % (path, a + 1, b + 1)] = text
        json.dump(data, open(out, 'w'), ensure_ascii=False, indent=1)
        print('extracted %d blocks -> %s' % (len(data), out))
    elif cmd == 'apply':
        orig = json.load(open(sys.argv[2]))
        trans = json.load(open(sys.argv[3]))
        byfile = {}
        for k, v in trans.items():
            byfile.setdefault(k.rsplit(':', 1)[0], []).append((k, v))
        total = miss = 0
        for path, items in byfile.items():
            s = open(path, encoding='utf-8').read()
            m = 0
            for k, v in items:
                o = orig.get(k)
                if o is None or o not in s:
                    m += 1
                    continue
                s = s.replace(o, v, 1)
            open(path, 'w', encoding='utf-8').write(s)
            total += len(items) - m
            miss += m
            print('%-72s ok=%d missing=%d' % (path, len(items) - m, m))
        print('TOTAL replaced=%d missing=%d' % (total, miss))
    else:
        print(__doc__)


if __name__ == '__main__':
    main()
