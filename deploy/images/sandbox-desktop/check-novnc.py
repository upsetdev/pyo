#!/usr/bin/env python3
"""Fails the image build when noVNC's page imports a file that isn't there.

A missing module leaves the page on its loading spinner, with no error.
usage: check-novnc.py /usr/share/novnc
"""

import os
import re
import sys

root = sys.argv[1]
IMPORT = re.compile(r"""(?:import|export)\s[^'"]*?from\s*['"](\.{1,2}/[^'"]+)['"]|import\s*['"](\.{1,2}/[^'"]+)['"]""")
missing = []
pages = ["vnc.html", "vnc_lite.html"]
for page in pages:
    text = open(os.path.join(root, page), encoding="utf-8").read()
    for m in re.finditer(r"""src=["']([^"':]+\.js)["']""", text):
        if not os.path.isfile(os.path.join(root, m.group(1))):
            missing.append(f"{page}: {m.group(1)}")
seen = 0
for dirpath, _, names in os.walk(root):
    for name in names:
        if not name.endswith((".js", ".html")):
            continue
        path = os.path.join(dirpath, name)
        seen += 1
        text = open(path, encoding="utf-8", errors="replace").read()
        for m in IMPORT.finditer(text):
            rel = m.group(1) or m.group(2)
            if not os.path.isfile(os.path.normpath(os.path.join(dirpath, rel))):
                missing.append(f"{os.path.relpath(path, root)}: {rel}")
if missing:
    sys.exit("noVNC is missing files it imports:\n  " + "\n  ".join(missing))
print(f"noVNC: every import of {seen} files is there")
