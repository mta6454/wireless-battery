#!/usr/bin/env python3
"""Writes the license notices of the Rust crates a build for one platform links against.

    python3 scripts/third-party-licenses.py --target x86_64-pc-windows-msvc -o installer/THIRD_PARTY_LICENSES.txt

The crate list comes from `cargo metadata --filter-platform <target>`, so each platform's file only
names the crates that platform really uses. The full text of every LICENSE / COPYING / NOTICE file
found in each crate is included once per distinct text, with the crates it applies to listed above it.
Run `cargo fetch` (or build once) first so the crate sources are on disk.
"""
import argparse
import hashlib
import json
import re
import subprocess
import sys
from collections import OrderedDict
from pathlib import Path

LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|notice|unlicense)([-_.].*)?$", re.I)
SKIP_SUFFIX = {".rs", ".toml", ".json", ".yml", ".yaml", ".sh", ".py"}


def metadata(target: str) -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target],
        check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def license_files(pkg_dir: Path) -> list:
    found = []
    for p in sorted(pkg_dir.iterdir()):
        if p.is_file() and LICENSE_FILE.match(p.name) and p.suffix.lower() not in SKIP_SUFFIX:
            found.append(p)
    return found


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", required=True, help="Rust target triple, e.g. x86_64-pc-windows-msvc")
    ap.add_argument("-o", "--output", required=True)
    args = ap.parse_args()

    md = metadata(args.target)
    root_ids = set(md["workspace_members"])
    used = {n["id"] for n in md["resolve"]["nodes"]}
    pkgs = sorted((p for p in md["packages"] if p["id"] in used and p["id"] not in root_ids), key=lambda p: (p["name"], p["version"]))

    texts = OrderedDict()  # sha -> (text, [pkg labels])
    summary, no_text = [], []
    for p in pkgs:
        label = f'{p["name"]} {p["version"]}'
        lic = p.get("license") or (f'see {p["license_file"]}' if p.get("license_file") else "unknown")
        summary.append((label, lic, p.get("repository") or ""))
        files = license_files(Path(p["manifest_path"]).parent)
        if not files:
            no_text.append(label)
        for f in files:
            try:
                text = f.read_text(encoding="utf-8", errors="replace").strip()
            except OSError:
                continue
            if not text:
                continue
            sha = hashlib.sha256(re.sub(r"\s+", " ", text).encode()).hexdigest()
            texts.setdefault(sha, (text, []))[1].append(label)

    out = []
    out.append(f"Third-party Rust crates linked into Wireless Battery ({args.target})\n")
    out.append("Wireless Battery itself is MIT-licensed (see LICENSE). The crates below are\nused under their own licenses, whose full texts follow the list.\n")
    out.append("=" * 78 + "\nCRATES\n" + "=" * 78)
    for label, lic, repo in summary:
        out.append(f"{label}\n    license: {lic}" + (f"\n    source:  {repo}" if repo else ""))
    out.append("")
    out.append("=" * 78 + "\nLICENSE TEXTS\n" + "=" * 78)
    for sha, (text, who) in texts.items():
        names = sorted(set(who))
        out.append("-" * 78)
        out.append("Applies to: " + ", ".join(names))
        out.append("-" * 78)
        out.append(text + "\n")
    if no_text:
        out.append("=" * 78)
        out.append("These crates ship no license file in their package; their license is the SPDX\nexpression given in the list above, and the text is available from their source link:")
        out.append(", ".join(no_text))
    Path(args.output).parent.mkdir(parents=True, exist_ok=True)
    Path(args.output).write_text("\n".join(out) + "\n", encoding="utf-8")
    print(f"{args.output}: {len(pkgs)} crates, {len(texts)} distinct license texts, {len(no_text)} crates without a license file", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
