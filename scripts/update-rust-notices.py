#!/usr/bin/env python3
"""Refreshes the Rust part of resources/licenses/ from the current Cargo tree.

gen-notices.py rebuilds everything from a staging dir collected once by hand
(MSYS, PDFium, npm, crates). When only Rust dependencies change, this script
is enough: run it on the build machine (Windows, cargo registry populated):

    python scripts/update-rust-notices.py

It lists the crates the shipped app links (`cargo tree -e normal` for the
Windows target; workspace members and proc-macro crates excluded), reads each crate's license files
from the registry, replaces the "core" group in third-party.json and rewrites
THIRD_PARTY_NOTICES.txt in the same layout gen-notices.py writes. The other
groups (semantic, pdf, ocr, ui) and their texts are kept as they are.
"""
import datetime, hashlib, json, os, subprocess, sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
OUT = os.path.join(ROOT, "resources", "licenses")
TARGET = "x86_64-pc-windows-msvc"
LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "unlicense", "copyright")

def run(*args):
    return subprocess.run(args, cwd=ROOT, check=True, capture_output=True, text=True, encoding="utf-8").stdout

meta = json.loads(run("cargo", "metadata", "--format-version", "1", "--filter-platform", TARGET))
members = set(meta["workspace_members"])
by_id = {p["id"]: p for p in meta["packages"]}
tree = run("cargo", "tree", "-e", "normal", "--target", TARGET, "-p", "zoria-arsiv", "--prefix", "none", "--format", "{p}")
wanted = set()
for line in tree.splitlines():
    parts = line.replace("(*)", "").split()
    if len(parts) >= 2 and parts[1].startswith("v"):
        wanted.add((parts[0], parts[1][1:]))
def proc_macro(p):
    # Build-time only: a proc-macro crate runs in the compiler, nothing of it ships.
    return any("proc-macro" in t.get("kind", []) for t in p.get("targets", []))

crates = sorted((p for p in meta["packages"]
                 if (p["name"], p["version"]) in wanted and p["id"] not in members and not proc_macro(p)),
                key=lambda p: (p["name"].lower(), p["version"]))

data = json.load(open(os.path.join(OUT, "third-party.json"), encoding="utf-8"))
texts = data["texts"]
standard = {v["title"].split(" (standard text)")[0]: k for k, v in texts.items() if v["title"].endswith("(standard text)")}

def tid(title, body):
    body = body.replace("\r\n", "\n").strip("\n")
    h = hashlib.sha1(body.encode()).hexdigest()[:12]
    texts.setdefault(h, {"title": title, "text": body})
    return h

def first_spdx(expr):
    for token in (expr or "").replace("(", " ").replace(")", " ").replace("/", " ").split():
        if token in standard:
            return token
    return None

core = []
for p in crates:
    folder = os.path.dirname(p["manifest_path"])
    files = sorted(f for f in os.listdir(folder) if f.lower().startswith(LICENSE_PREFIXES) and os.path.isfile(os.path.join(folder, f)))
    ts = [tid(f"{p['name']} — {f}", open(os.path.join(folder, f), encoding="utf-8", errors="replace").read()) for f in files]
    note = note_tr = ""
    if not ts:
        key = first_spdx(p.get("license")) or "MIT"
        if key not in standard:
            sys.exit(f"{p['name']}: no license file and no standard text for {p.get('license')!r}")
        ts = [standard[key]]
        note = "The package ships no license file; the standard text of its declared license is included."
        note_tr = "Paket ayrı lisans dosyası içermiyor; beyan edilen lisansın standart metni eklendi."
    core.append({"name": p["name"], "version": p["version"], "license": p.get("license") or "",
                 "url": p.get("repository") or "", "note": note, "note_tr": note_tr, "texts": ts})

old = {(i["name"], i["version"]) for g in data["groups"] if g["id"] == "core" for i in g["items"]}
new = {(i["name"], i["version"]) for i in core}
for g in data["groups"]:
    if g["id"] == "core":
        g["items"] = core
used = {t for g in data["groups"] for i in g["items"] for t in i["texts"]}
data["texts"] = {k: v for k, v in texts.items() if k in used}
data["generated"] = datetime.date.today().isoformat()
json.dump(data, open(os.path.join(OUT, "third-party.json"), "w", encoding="utf-8"), ensure_ascii=False, separators=(",", ":"))

with open(os.path.join(OUT, "THIRD_PARTY_NOTICES.txt"), "w", encoding="utf-8", newline="\r\n") as w:
    w.write("ZORIA ARŞİV — THIRD-PARTY COMPONENTS AND THEIR LICENSES\n")
    w.write("Zoria Arşiv includes the open-source components below. Each is covered by its own license.\n")
    w.write("LGPL/GPL components are distributed unmodified as separate DLL files and are not linked\n"
            "into Zoria Arşiv code; their source code is at the addresses given and is also available\n"
            "from ZoriaSoft on request.\n\n")
    for a in data["acknowledgements"]:
        w.write(a + "\n")
    for g in data["groups"]:
        w.write("\n" + "=" * 78 + "\n" + g["title"].upper() + "\n" + "=" * 78 + "\n")
        for it in g["items"]:
            w.write(f"\n- {it['name']} {it['version']}".rstrip() + f"  [{it['license']}]\n")
            if it.get("url"): w.write(f"  {it['url']}\n")
            if it.get("note"): w.write(f"  {it['note']}\n")
            w.write("  License text: " + ", ".join("#" + t for t in it["texts"]) + "\n")
    w.write("\n" + "=" * 78 + "\nLICENSE TEXTS\n" + "=" * 78 + "\n")
    for k, v in data["texts"].items():
        w.write(f"\n----- #{k} · {v['title']} -----\n\n{v['text']}\n")

print(f"core: {len(core)} crates; added {sorted(n for n in new - old)}; removed {sorted(n for n in old - new)}")
