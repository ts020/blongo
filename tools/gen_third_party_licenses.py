#!/usr/bin/env python3
"""Write THIRD_PARTY_LICENSES.txt: the license texts of every crate linked
into the shipped binaries (blongo, blongo-serve), for distribution with
them.

Usage: tools/gen_third_party_licenses.py [OUTPUT]   (default: THIRD_PARTY_LICENSES.txt)

Reads `cargo metadata` for Linux, Windows and macOS targets, walks normal
(non-dev, non-build) dependencies from the two binaries' packages, and
copies each crate's LICENSE* / LICENCE* / COPYING* / NOTICE* / UNLICENSE
files. Identical texts are written once with every crate that ships them.
A crate without a license file is listed with its SPDX expression.
Offline: only the local cargo registry / git checkouts are read.
"""
import json
import os
import subprocess
import sys

ROOTS = {"blongo", "blongo-server"}
TARGETS = ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"]
PREFIXES = ("license", "licence", "copying", "notice", "unlicense")


MIT = """MIT License

Copyright (c) the authors of the crate

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE."""

ZLIB = """zlib License

Copyright (c) the authors of the crate

This software is provided 'as-is', without any express or implied warranty.
In no event will the authors be held liable for any damages arising from the
use of this software.

Permission is granted to anyone to use this software for any purpose,
including commercial applications, and to alter it and redistribute it
freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not
   claim that you wrote the original software. If you use this software in a
   product, an acknowledgment in the product documentation would be
   appreciated but is not required.
2. Altered source versions must be plainly marked as such, and must not be
   misrepresented as being the original software.
3. This notice may not be removed or altered from any source distribution."""


def metadata(target):
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--offline",
         "--filter-platform", target],
        check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def linked(meta):
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    members = set(meta["workspace_members"])
    start = [p["id"] for p in meta["packages"] if p["name"] in ROOTS and p["id"] in members]
    seen, stack = set(), list(start)
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    return seen - members


def license_files(manifest_dir):
    found = []
    try:
        names = sorted(os.listdir(manifest_dir))
    except OSError:
        return found
    for name in names:
        path = os.path.join(manifest_dir, name)
        if name.lower().startswith(PREFIXES) and os.path.isfile(path):
            try:
                with open(path, encoding="utf-8", errors="replace") as f:
                    text = f.read().strip()
            except OSError:
                continue
            if text:
                found.append((name, text))
    return found


def main():
    out_path = sys.argv[1] if len(sys.argv) > 1 else "THIRD_PARTY_LICENSES.txt"
    packages = {}
    for target in TARGETS:
        meta = metadata(target)
        by_id = {p["id"]: p for p in meta["packages"]}
        for pid in linked(meta):
            packages[pid] = by_id[pid]
    texts = {}  # text -> list of "name version (file)"
    bare = []
    for p in sorted(packages.values(), key=lambda p: (p["name"], p["version"])):
        label = f'{p["name"]} {p["version"]}'
        files = license_files(os.path.dirname(p["manifest_path"]))
        if not files:
            bare.append(f'{label}: {p.get("license") or "see its repository"}')
            continue
        for name, text in files:
            texts.setdefault(text, []).append(f"{label} ({name})")
    with open(out_path, "w") as f:
        f.write(
            "Third-party software in Blongo\n"
            "==============================\n\n"
            f"Generated by tools/gen_third_party_licenses.py from Cargo.lock: "
            f"{len(packages)} crates linked into blongo and blongo-serve on "
            "Linux, macOS and Windows. Each license text below is followed by "
            "the crates that ship it. Where a crate offers a choice of licenses, "
            "Blongo uses a permissive one (checked by `cargo deny check "
            "licenses`); the crate's other license files are copied as shipped. "
            "See THIRD_PARTY_NOTICES.md for the summary table.\n\n"
        )
        if bare:
            f.write("Crates without a license file in their package (license by SPDX expression):\n")
            for line in bare:
                f.write(f"  {line}\n")
            apache = next((u[0] for t, u in sorted(texts.items(), key=lambda kv: kv[1][0])
                           if "Apache License" in t and "Version 2.0, January 2004" in t), None)
            f.write(
                "\nFor these, the standard texts apply: MIT and Zlib below; Apache-2.0 as "
                f"printed with {apache}; CC0-1.0 at "
                "https://creativecommons.org/publicdomain/zero/1.0/legalcode. The "
                "copyright holders are the crates' authors as listed in their "
                "Cargo.toml.\n\n"
            )
            f.write(MIT + "\n\n" + ZLIB + "\n\n")
        for text, users in sorted(texts.items(), key=lambda kv: kv[1][0]):
            f.write("=" * 78 + "\n")
            for u in users:
                f.write(f"{u}\n")
            f.write("-" * 78 + "\n")
            f.write(text + "\n\n")
    print(f"{out_path}: {len(packages)} crates, {len(texts)} distinct texts, {len(bare)} without a file")


if __name__ == "__main__":
    main()
