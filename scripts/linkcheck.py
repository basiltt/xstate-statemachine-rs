#!/usr/bin/env python3
"""Check the relative links of a built mdBook (stdlib only).

Usage: ``python3 scripts/linkcheck.py docs/book/book``. Every relative ``<a href>`` and
``<img src>`` must resolve to a file in the build; external schemes and fragments are ignored.
Root-relative references are reported (they cannot be checked here) but do not fail the run.
"""

import os
import re
import sys
from html.parser import HTMLParser


class RefParser(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.refs = []

    def handle_starttag(self, tag, attrs):
        d = dict(attrs)
        if tag == "a" and d.get("href"):
            self.refs.append(("a", d["href"]))
        elif tag == "img" and d.get("src"):
            self.refs.append(("img", d["src"]))


SCHEME = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.-]*:")


def main():
    book_root = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "docs/book/book")
    missing = []
    root_relative = []
    checked = 0
    files = 0

    for dirpath, _dirs, names in os.walk(book_root):
        for name in names:
            if not name.endswith(".html"):
                continue
            files += 1
            path = os.path.join(dirpath, name)
            with open(path, encoding="utf-8", errors="replace") as fh:
                content = fh.read()
            parser = RefParser()
            parser.feed(content)
            for kind, url in parser.refs:
                url = url.strip()
                if not url or SCHEME.match(url) or url.startswith("//") or url.startswith("#"):
                    continue
                if url.startswith("/"):
                    root_relative.append((os.path.relpath(path, book_root), kind, url))
                    continue
                target = url.split("#", 1)[0].split("?", 1)[0]
                if not target:
                    continue
                target_path = os.path.normpath(os.path.join(dirpath, target))
                checked += 1
                if not os.path.exists(target_path):
                    missing.append((os.path.relpath(path, book_root), kind, url, target_path))

    print(f"scanned {files} html files, checked {checked} relative refs")
    if root_relative:
        print(f"root-relative refs (unverifiable against {book_root}): {len(root_relative)}")
        for rel, kind, url in root_relative:
            print(f"  {rel}: <{kind}> {url}")
    if missing:
        print(f"MISSING: {len(missing)}")
        for rel, kind, url, target in missing:
            print(f"  {rel}: <{kind} href/src=\"{url}\"> -> {target}")
    else:
        print("no missing relative targets")
    sys.exit(1 if missing else 0)


if __name__ == "__main__":
    main()
