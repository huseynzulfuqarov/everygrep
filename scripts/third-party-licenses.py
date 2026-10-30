#!/usr/bin/env python3
"""Writes THIRD-PARTY-LICENSES.md: the crates EveryGrep's programs are built
from, with their licenses.

Run from anywhere with `python3 -I scripts/third-party-licenses.py`. It uses
only Cargo, offline and with the locked versions (`cargo tree` for the crate
set, `cargo metadata --format-version 1 --locked` for licenses, repositories
and source folders), and the license files inside each crate's source folder
(for crates.io crates, in ~/.cargo/registry/src). Run `cargo fetch --locked`
first if the sources are missing.

Listed are the crates reached from everygrep-cli and everygrep-gui through
normal dependencies on x86_64-unknown-linux-gnu, with the features the build
enables: build scripts' own dependencies and dev-dependencies are left out,
procedural macros (normal dependencies, run while compiling) are kept and
marked. Identical license texts are printed once, with every crate that
ships them.
"""

import hashlib
import json
import os
import re
import subprocess
import sys

TARGET = "x86_64-unknown-linux-gnu"
ROOTS = ("everygrep-cli", "everygrep-gui")
OUTPUT = "THIRD-PARTY-LICENSES.md"
LICENSE_FILE = re.compile(r"^(LICEN[CS]E|COPYING|NOTICE|UNLICENSE)([-_.].*)?$", re.IGNORECASE)
COPYLEFT = re.compile(r"\b(A?GPL|LGPL|MPL|EPL|CDDL|EUPL|OSL|CC-BY-SA)", re.IGNORECASE)


def metadata(root):
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--offline", "--filter-platform", TARGET],
        cwd=root, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def tree(root, edges):
    """(name, version) of the crates `cargo tree` reaches from ROOTS over the
    given edge kinds. Unlike the resolve graph of `cargo metadata`, which
    merges the features of every use, `cargo tree` resolves features the way
    the build does, so crates that are never compiled are left out."""
    args = ["cargo", "tree", "--locked", "--offline", "-e", edges, "--target", TARGET,
            "--prefix", "none", "--format", "{p}"]
    for name in ROOTS:
        args += ["-p", name]
    out = subprocess.run(args, cwd=root, check=True, capture_output=True, text=True).stdout
    found = set()
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[1].startswith("v"):
            found.add((parts[0], parts[1][1:]))
    return found


def shipped_packages(root, meta):
    """Packages the programs are built from, and the ones among them that
    are linked into the programs (the rest are procedural macros and their
    dependencies, which only run while compiling)."""
    built = tree(root, "normal")
    linked = tree(root, "normal,no-proc-macro")
    members = set(meta["workspace_members"])
    packages = [p for p in meta["packages"]
                if (p["name"], p["version"]) in built and p["id"] not in members]
    if len(packages) != len(built) - len(ROOTS) - 1:
        sys.exit(f"third-party-licenses: {len(built)} crates in the tree, {len(packages)} found")
    packages.sort(key=lambda p: (p["name"].lower(), p["version"]))
    return packages, linked


def license_files(package):
    """(relative path, text) of the license files in a crate's folder: files
    named LICENSE*, LICENCE*, COPYING*, NOTICE* at its top level, and the files
    in a LICENSES/ folder (REUSE layout), plus a license-file from Cargo.toml."""
    folder = os.path.dirname(package["manifest_path"])
    found = []
    for name in sorted(os.listdir(folder)):
        path = os.path.join(folder, name)
        if os.path.isfile(path) and LICENSE_FILE.match(name):
            found.append(name)
        elif os.path.isdir(path) and name.upper() in ("LICENSES", "LICENCES"):
            found += [os.path.join(name, f) for f in sorted(os.listdir(path))
                      if os.path.isfile(os.path.join(path, f))]
    extra = package.get("license_file")
    if extra and extra not in found and os.path.isfile(os.path.join(folder, extra)):
        found.append(extra)
    out = []
    for rel in found:
        with open(os.path.join(folder, rel), encoding="utf-8", errors="replace") as f:
            out.append((rel, f.read()))
    return out


def normalized(text):
    """Text for comparing licenses: line ends and trailing blanks ignored."""
    return "\n".join(line.rstrip() for line in text.strip().replace("\r\n", "\n").split("\n"))


def title_of(rel, text):
    first = next((l.strip(" #*=") for l in text.strip().splitlines() if l.strip(" #*=-")), "")
    return f"{os.path.basename(rel)}: {first[:70]}" if first else os.path.basename(rel)


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    meta = metadata(root)
    crates, linked = shipped_packages(root, meta)

    texts = {}  # hash -> {"text", "title", "crates": []}
    rows, unknown, copyleft = [], [], []
    expressions = {}
    for p in crates:
        name = f"{p['name']} {p['version']}"
        expr = p.get("license") or ""
        if not expr:
            expr = f"see {p['license_file']}" if p.get("license_file") else "UNKNOWN"
            unknown.append(name)
        expressions[expr] = expressions.get(expr, 0) + 1
        if COPYLEFT.search(expr):
            copyleft.append((name, expr))
        files = license_files(p)
        if not files:
            unknown.append(f"{name} (no license file in the crate; the expression is known)")
        for rel, text in files:
            key = hashlib.sha256(normalized(text).encode()).hexdigest()[:12]
            entry = texts.setdefault(key, {"text": text.strip(), "title": title_of(rel, text), "crates": []})
            entry["crates"].append(name)
        repo = p.get("repository") or ""
        refs = ", ".join(
            f"[{k}](#license-{k})"
            for k in dict.fromkeys(
                hashlib.sha256(normalized(t).encode()).hexdigest()[:12] for _, t in files
            )
        ) or "standard text of the license"
        use = "linked" if (p["name"], p["version"]) in linked else "compile time"
        rows.append(f"| {p['name']} | {p['version']} | {expr} | {use} | {repo} | {refs} |")

    lines = [
        "# Third-party licenses",
        "",
        "EveryGrep's programs (`everygrep` and `everygrep-gui`) are built from the",
        "crates below. This file is generated by `scripts/third-party-licenses.py`",
        "from `cargo metadata` and the license files shipped inside each crate; do",
        "not edit it by hand. It lists the crates reached through normal",
        f"dependencies on `{TARGET}`.",
        "",
        "The window uses the [Slint](https://slint.dev) toolkit under its",
        "royalty-free license (`LicenseRef-Slint-Royalty-free-2.0`), whose",
        "attribution is shown in Help → About EveryGrep.",
        "",
        f"Crates: {len(crates)} ({sum(1 for p in crates if (p['name'], p['version']) in linked)}"
        f" linked into the programs). Distinct license texts: {len(texts)}.",
        "",
        "## License expressions",
        "",
        "| Expression | Crates |",
        "|---|---|",
    ]
    for expr, n in sorted(expressions.items(), key=lambda e: (-e[1], e[0])):
        lines.append(f"| {expr} | {n} |")
    lines += ["", "## Crates", "",
              "\"Linked\" crates are part of the programs; \"compile time\" crates are",
              "procedural macros and their dependencies, which run only while building.",
              "",
              "| Crate | Version | License | Use | Repository | License texts |",
              "|---|---|---|---|---|---|"]
    lines += rows
    lines += ["", "## License texts", ""]
    for key, entry in sorted(texts.items(), key=lambda e: (e[1]["title"].lower(), e[0])):
        fence = "```"
        while fence in entry["text"]:
            fence += "`"
        lines += [
            f'<a id="license-{key}"></a>',
            f"### {entry['title']}",
            "",
            "Used by: " + ", ".join(entry["crates"]),
            "",
            fence + "text",
            entry["text"],
            fence,
            "",
        ]
    with open(os.path.join(root, OUTPUT), "w", encoding="utf-8") as f:
        f.write("\n".join(lines))

    print(f"{OUTPUT}: {len(crates)} crates, {len(texts)} distinct license texts")
    for expr, n in sorted(expressions.items(), key=lambda e: (-e[1], e[0])):
        print(f"  {n:4}  {expr}")
    for name, expr in copyleft:
        print(f"copyleft option: {name}: {expr}")
    for name in unknown:
        print(f"note: {name}")


if __name__ == "__main__":
    main()
