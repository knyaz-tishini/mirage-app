#!/usr/bin/env python3
"""Keep the newest desktop OTA releases on disk.

The cap counts release versions, not files. Each kept version keeps every
platform payload inside it. ``stable/`` is the track pointer and is never
removed. Versions older than the three newest are deleted.
"""

import shutil
import sys
from pathlib import Path

KEEP = 3
ROOT = Path("/var/lib/mirage2site/landing/ota/desktop")


def version_key(name: str):
    parts = name.split(".")
    if len(parts) != 3 or not all(part.isdigit() for part in parts):
        return None
    return tuple(int(part) for part in parts)


def main() -> int:
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT
    if not root.is_dir():
        print(f"missing {root}", file=sys.stderr)
        return 1
    found = []
    for child in root.iterdir():
        if not child.is_dir():
            continue
        key = version_key(child.name)
        if key is not None:
            found.append((key, child))
    found.sort(reverse=True)
    kept = [child.name for _, child in found[:KEEP]]
    for _, child in found[KEEP:]:
        shutil.rmtree(child)
        print(f"dropped {child.name}")
    print("kept " + " ".join(kept))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
