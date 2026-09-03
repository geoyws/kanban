#!/usr/bin/env python3
from pathlib import Path
import sys


def main() -> int:
    if len(sys.argv) != 4:
        raise SystemExit("usage: inject-text.py SOURCE NEEDLE REPLACEMENT_FILE")

    source = Path(sys.argv[1])
    needle = sys.argv[2]
    replacement_file = Path(sys.argv[3])
    replacement = replacement_file.read_text()
    text = source.read_text()
    if needle not in text:
        raise SystemExit("missing expected literal")
    source.write_text(text.replace(needle, replacement, 1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
