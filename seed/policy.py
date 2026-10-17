#!/usr/bin/env python3
"""Seed context-injection policy — the baseline the search starts from.

Reads the harness store (gray-learn's folded-in memory) plus loose
memory/*.md / learn/*.md files, ranks entries by keyword overlap with
recent_user_text, and injects the top few. Pure stdlib, ~instant, and
defensive: any failure prints an empty inject and exits 0 so a broken
policy never breaks the agent.
"""

import json
import re
import sys
from pathlib import Path

MAX_INJECT = 8
MAX_CHARS = 3000
WORD = re.compile(r"[a-z0-9_]{4,}")


def words(text):
    return set(WORD.findall((text or "").lower()))


def harness_entries(home):
    """Entries from harness/state.json — the kinded store gray-learn writes."""
    path = home / "harness" / "state.json"
    try:
        data = json.loads(path.read_text())
    except Exception:
        return
    for kind, entries in (data.get("entries") or {}).items():
        if not isinstance(entries, dict):
            continue
        for entry in entries.values():
            if not isinstance(entry, dict):
                continue
            title = entry.get("title") or kind
            text = (entry.get("content") or "").strip()
            if text:
                yield title, text


def md_files(home):
    """Loose markdown under memory/ and learn/ — filename is the title."""
    for sub in ("memory", "learn"):
        d = home / sub
        if not d.is_dir():
            continue
        try:
            files = sorted(d.glob("*.md"))
        except Exception:
            continue
        for f in files:
            try:
                text = f.read_text(errors="replace").strip()
            except Exception:
                continue
            if text:
                yield f"{sub}/{f.stem}", text


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception:
        payload = {}
    home = Path(payload.get("gray_home") or "").expanduser()
    if not str(home):
        print(json.dumps({"inject": []}))
        return

    recent = words(payload.get("recent_user_text"))
    inject, used = [], 0

    def ranked():
        for title, text in list(harness_entries(home)) + list(md_files(home)):
            overlap = len(recent & words(text)) + len(recent & words(title))
            yield overlap, title, text

    for overlap, title, text in sorted(ranked(), reverse=True):
        if len(inject) >= MAX_INJECT or used >= MAX_CHARS:
            break
        if recent and overlap == 0:
            continue
        inject.append({"title": title, "text": text[:MAX_CHARS]})
        used += len(text)

    print(json.dumps({"inject": inject, "notes": "seed: keyword-ranked recall"}))


if __name__ == "__main__":
    try:
        main()
    except Exception:
        # Fail open: a crashed policy injects nothing rather than breaking the turn.
        print(json.dumps({"inject": []}))
