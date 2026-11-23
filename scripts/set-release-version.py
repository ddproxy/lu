#!/usr/bin/env python3
"""Make the build version agree with the release tag.

A stable tag (v0.6.2) must equal the workspace version in Cargo.toml, so a tag cannot ship a
binary that reports another version. A tag with a pre-release part (v0.6.2-dev.7.g1a2b3c4) is an
automatic dev build: the workspace version in Cargo.toml and the workspace members in Cargo.lock
are rewritten in the checkout, never committed, so `lu --version` reports the dev version.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SEMVER = re.compile(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$")


def main():
    if len(sys.argv) != 2 or not sys.argv[1].startswith("v"):
        sys.exit("usage: set-release-version.py vX.Y.Z[-pre]")
    version = sys.argv[1][1:]
    if not SEMVER.match(version):
        sys.exit(f"not a version tag: {sys.argv[1]}")

    manifest = (ROOT / "Cargo.toml").read_text()
    match = re.search(r"(?ms)^\[workspace\.package\].*?^version = \"([^\"]+)\"", manifest)
    if not match:
        sys.exit("no [workspace.package] version in Cargo.toml")
    current = match.group(1)

    if "-" not in version:
        if version != current:
            sys.exit(f"tag {sys.argv[1]} does not match the Cargo.toml version {current}")
        print(f"release version {version}")
        return

    start, end = match.span(1)
    (ROOT / "Cargo.toml").write_text(manifest[:start] + version + manifest[end:])
    lock_path = ROOT / "Cargo.lock"
    lock = lock_path.read_text()

    def bump(block):
        # Workspace members have no `source` line; only they carry the workspace version.
        if "\nsource = " in block or f'\nversion = "{current}"\n' not in block + "\n":
            return block
        return block.replace(f'version = "{current}"', f'version = "{version}"', 1)

    blocks = lock.split("\n\n")
    lock_path.write_text("\n\n".join(bump(b) for b in blocks))
    print(f"dev build version {version} (was {current})")


if __name__ == "__main__":
    main()
