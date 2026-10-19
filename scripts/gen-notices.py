#!/usr/bin/env python3
"""Builds resources/licenses/{third-party.json,THIRD_PARTY_NOTICES.txt}.

Inputs live in a staging dir (default ./lic-staging, never committed):
  rustlic.json   crates (cargo tree -e normal, Windows target) + npm runtime deps
                 with their license files, collected on the build machine
  msys.json + pk/ MSYS2 mingw64 packages behind resources/bin (license files)
  pdfium/        LICENSE + licenses/ from the pdfium-binaries archive
  spdx/          SPDX standard texts (for packages that ship no license file)
  extra/         icu.txt lz4.txt giflib.txt ort-LICENSE.txt ort-TPN.txt tessdata.txt
Re-run whenever a dependency or a bundled binary changes.

Groups and notes are written in English (`title`, `note`) with a Turkish
copy (`title_tr`, `note_tr`) for the Turkish UI; license texts stay as their
authors wrote them. THIRD_PARTY_NOTICES.txt is English.
"""
import hashlib, json, os, sys, datetime

S = sys.argv[1] if len(sys.argv) > 1 else "lic-staging"
OUT = os.path.join(os.path.dirname(__file__), "..", "resources", "licenses")
texts = {}

def rd(p):
    return open(os.path.join(S, p), encoding="utf-8", errors="replace").read()

def tid(title, body):
    body = body.replace("\r\n", "\n").strip("\n")
    h = hashlib.sha1(body.encode()).hexdigest()[:12]
    texts.setdefault(h, {"title": title, "text": body})
    return h

SPDX = {"Apache-2.0": "Apache-2.0", "MIT": "MIT", "BSD-3-Clause": "BSD-3-Clause",
        "BSD-2-Clause": "BSD-2-Clause", "MPL-2.0": "MPL-2.0", "ISC": "ISC", "Zlib": "Zlib",
        "CC0-1.0": "CC0-1.0", "Unicode-3.0": "Unicode-3.0", "GPL-2.0": "GPL-2.0-only",
        "GPL-3.0": "GPL-3.0-only", "LGPL-2.1": "LGPL-2.1-only", "LGPL-3.0": "LGPL-3.0-only",
        "GCC-exception-3.1": "GCC-exception-3.1"}

def spdx(i):
    return tid(i + " (standard text)", rd(f"spdx/{SPDX[i]}.txt"))

def first_spdx(expr):
    for k in SPDX:
        if k in expr:
            return k
    return None

groups = []
rl = json.load(open(os.path.join(S, "rustlic.json"), encoding="utf-8"))

# ── Semantic search ────────────────────────────────────────────────────
groups.append({"id": "semantic", "title": "Semantic search", "title_tr": "Anlam araması", "items": [
    {"name": "EmbeddingGemma 2 (text encoder, q4 ONNX)", "version": "daa72c5", "license": "Apache-2.0",
     "url": "https://huggingface.co/onnx-community/embeddinggemma-2-ONNX",
     "note": "Google DeepMind. Use is subject to the Gemma Prohibited Use Policy: https://ai.google.dev/gemma/prohibited_use_policy",
     "note_tr": "Google DeepMind. Kullanımı Gemma Yasaklı Kullanım Politikası'na tabidir: https://ai.google.dev/gemma/prohibited_use_policy",
     "texts": [spdx("Apache-2.0")]},
    {"name": "ONNX Runtime", "version": "1.31.0", "license": "MIT", "url": "https://github.com/microsoft/onnxruntime",
     "texts": [tid("ONNX Runtime — LICENSE", rd("extra/ort-LICENSE.txt")),
               tid("ONNX Runtime — Third Party Notices", rd("extra/ort-TPN.txt"))]},
]})

# ── PDF ────────────────────────────────────────────────────────────────
pd = [{"name": "PDFium", "version": "157.0.8100", "license": "BSD-3-Clause AND Apache-2.0",
       "url": "https://pdfium.googlesource.com/pdfium/", "texts": [tid("PDFium", rd("pdfium/licenses/pdfium.txt"))]},
      {"name": "pdfium-binaries (build)", "version": "chromium/8100", "license": "MIT",
       "url": "https://github.com/bblanchon/pdfium-binaries", "texts": [tid("pdfium-binaries", rd("pdfium/LICENSE"))]}]
PDFC = [("abseil.txt", "Abseil", "Apache-2.0"), ("agg23.txt", "Anti-Grain Geometry 2.3", "BSD-3-Clause"),
        ("dragonbox-Apache2-LLVM.txt", "Dragonbox", "Apache-2.0 WITH LLVM-exception OR BSL-1.0"),
        ("dragonbox-Boost.txt", None, None), ("fast_float.txt", "fast_float", "MIT OR Apache-2.0 OR BSL-1.0"),
        ("freetype.txt", "FreeType", "FTL"), ("harfbuzz.txt", "HarfBuzz", "MIT"), ("icu.txt", "ICU", "Unicode-3.0"),
        ("lcms.txt", "Little CMS", "MIT"), ("libjpeg_turbo.ijg", "libjpeg-turbo", "IJG AND BSD-3-Clause"),
        ("libjpeg_turbo.md", None, None), ("libopenjpeg.txt", "OpenJPEG", "BSD-2-Clause"),
        ("libpng.txt", "libpng", "libpng-2.0"), ("llvm-libc.txt", "LLVM libc", "Apache-2.0 WITH LLVM-exception"),
        ("simdutf.txt", "simdutf", "MIT OR Apache-2.0"), ("zlib.txt", "zlib", "Zlib")]
for f, name, lic in PDFC:
    t = tid(f"Inside PDFium — {f}", rd(f"pdfium/licenses/{f}"))
    if name:
        pd.append({"name": name, "version": "", "license": lic, "url": "", "note": "Compiled into PDFium",
                   "note_tr": "PDFium içinde derlenmiş", "texts": [t]})
    else:
        pd[-1]["texts"].append(t)
groups.append({"id": "pdf", "title": "PDF reading", "title_tr": "PDF okuma", "items": pd})

# ── OCR (Tesseract, UB Mannheim Windows build of MSYS2 mingw64 packages) ─
msys = json.load(open(os.path.join(S, "msys.json")))
DLL = {  # package -> (display, bundled version if the binaries report it, dlls)
 "tesseract-ocr": ("Tesseract OCR", "5.5.3", "tesseract.exe, libtesseract-5.dll"),
 "leptonica": ("Leptonica", "1.87.0", "libleptonica-6.dll"),
 "libarchive": ("libarchive", "3.8.8", "libarchive-13.dll"), "libb2": ("BLAKE2 (libb2)", "", "libb2-1.dll"),
 "brotli": ("Brotli", "1.2.0", "libbrotlicommon.dll, libbrotlidec.dll"), "bzip2": ("bzip2", "1.0.8", "libbz2-1.dll"),
 "cairo": ("cairo", "", "libcairo-2.dll"), "curl": ("libcurl", "8.21.0", "libcurl-4.dll"),
 "libdatrie": ("libdatrie", "", "libdatrie-1.dll"), "libdeflate": ("libdeflate", "", "libdeflate.dll"),
 "expat": ("Expat", "2.8.2", "libexpat-1.dll"), "libffi": ("libffi", "", "libffi-8.dll"),
 "fontconfig": ("Fontconfig", "", "libfontconfig-1.dll"), "freetype": ("FreeType", "2.14.3", "libfreetype-6.dll"),
 "fribidi": ("GNU FriBidi", "", "libfribidi-0.dll"),
 "gcc-libs": ("GCC runtime libraries", "", "libgcc_s_seh-1.dll, libstdc++-6.dll"),
 "giflib": ("GIFLIB", "5.2.2", "libgif-7.dll"),
 "glib2": ("GLib", "2.88.2", "libglib-2.0-0.dll, libgio-2.0-0.dll, libgmodule-2.0-0.dll, libgobject-2.0-0.dll"),
 "graphite2": ("Graphite2", "", "libgraphite2.dll"), "harfbuzz": ("HarfBuzz", "", "libharfbuzz-0.dll"),
 "libiconv": ("GNU libiconv", "1.19", "libiconv-2.dll"), "icu": ("ICU", "78", "libicudt78.dll, libicuin78.dll, libicuuc78.dll"),
 "libidn2": ("GNU Libidn2", "2.3.8", "libidn2-0.dll"), "gettext-runtime": ("GNU gettext (libintl)", "1.0", "libintl-8.dll"),
 "jbigkit": ("JBIG-KIT", "", "libjbig-0.dll"), "libjpeg-turbo": ("libjpeg-turbo", "3.1.3", "libjpeg-8.dll"),
 "lerc": ("LERC", "", "libLerc.dll"), "lz4": ("LZ4", "1.10.0", "liblz4.dll"), "xz": ("XZ Utils (liblzma)", "5.8.3", "liblzma-5.dll"),
 "openjpeg2": ("OpenJPEG", "2.5.4", "libopenjp2-7.dll"),
 "pango": ("Pango", "1.58.0", "libpango-1.0-0.dll, libpangocairo-1.0-0.dll, libpangoft2-1.0-0.dll, libpangowin32-1.0-0.dll"),
 "pcre2": ("PCRE2", "", "libpcre2-8-0.dll"), "pixman": ("Pixman", "", "libpixman-1-0.dll"),
 "libpng": ("libpng", "1.6.58", "libpng16-16.dll"), "libpsl": ("libpsl", "0.21.5", "libpsl-5.dll"),
 "libwebp": ("libwebp", "1.6.0", "libwebp-7.dll, libwebpmux-3.dll, libsharpyuv-0.dll"),
 "libssh2": ("libssh2", "1.11.1", "libssh2-1.dll"), "libthai": ("LibThai", "", "libthai-0.dll"),
 "libtiff": ("LibTIFF", "4.7.2", "libtiff-6.dll"), "libunistring": ("GNU libunistring", "1.4.2", "libunistring-5.dll"),
 "libwinpthread": ("winpthreads", "", "libwinpthread-1.dll"), "zstd": ("Zstandard", "1.5.7", "libzstd.dll"),
 "zlib": ("zlib", "", "zlib1.dll"),
}
EXTRA = {"gcc-libs": lambda: [spdx("GPL-3.0"), spdx("GCC-exception-3.1")],
         "giflib": lambda: [tid("GIFLIB", "Copyright (c) 1997 Eric S. Raymond\n\n" + rd("extra/giflib.txt"))],
         "icu": lambda: [tid("ICU", rd("extra/icu.txt"))],
         "libidn2": lambda: [spdx("LGPL-3.0"), spdx("GPL-2.0")],
         "lz4": lambda: [tid("LZ4 (lib)", rd("extra/lz4.txt"))]}
COPYLEFT = {"cairo", "libdatrie", "fribidi", "gcc-libs", "glib2", "graphite2", "libiconv", "libidn2",
            "gettext-runtime", "jbigkit", "xz", "pango", "libthai", "libunistring"}
ocr = []
for pkg, (disp, ver, dlls) in DLL.items():
    m = msys[pkg]
    ts = []
    for f in sorted(m["files"]):
        rel = os.path.relpath(f, "pk")
        ts.append(tid(f"{disp} — {os.path.basename(f)}", rd(os.path.join("pk", rel))))
    if pkg in EXTRA:
        ts += EXTRA[pkg]()
    lic = ", ".join(x.replace("spdx:", "") for x in m["license"] if not x.startswith("documentation"))
    if pkg == "lz4": lic = "BSD-2-Clause (library)"
    note, note_tr = "Files: " + dlls, "Dosyalar: " + dlls
    if pkg in COPYLEFT:
        src = "https://github.com/msys2/MINGW-packages/tree/master/mingw-w64-" + pkg
        note += ". Distributed unmodified as separate DLLs; source code: " + src
        note_tr += ". Değiştirilmeden, ayrı DLL olarak dağıtılır; kaynak kodu: " + src
    ocr.append({"name": disp, "version": ver, "license": lic, "url": m["url"], "note": note, "note_tr": note_tr, "texts": ts})
ocr.insert(1, {"name": "Language data (tessdata_best tur, eng)", "version": "4.1.0", "license": "Apache-2.0",
               "url": "https://github.com/tesseract-ocr/tessdata_best",
               "note": "Files: tur.traineddata, eng.traineddata", "note_tr": "Dosyalar: tur.traineddata, eng.traineddata",
               "texts": [tid("tessdata_best", rd("extra/tessdata.txt"))]})
ocr[0]["note"] += ". Windows build: Universitätsbibliothek Mannheim (https://github.com/UB-Mannheim/tesseract)"
ocr[0]["note_tr"] += ". Windows derlemesi: Universitätsbibliothek Mannheim (https://github.com/UB-Mannheim/tesseract)"
groups.append({"id": "ocr", "title": "Text recognition (OCR)", "title_tr": "Metin tanıma (OCR)", "items": ocr})

# ── UI (npm runtime deps) ───────────────────────────────────────────────
ui = []
for n in rl["npm"]:
    ts = [tid(f"{n['name']} — {f}", t) for f, t in sorted(n["files"].items()) if not f.endswith(".spdx")]
    if not ts:  # @tauri-apps plugins ship only LICENSE.spdx: same texts as @tauri-apps/api
        ts = [spdx("MIT"), spdx("Apache-2.0")]
    rep = (n["repository"] or "").replace("git+", "").removesuffix(".git")
    ui.append({"name": n["name"], "version": n["version"], "license": n["license"], "url": rep, "texts": ts})
groups.append({"id": "ui", "title": "User interface", "title_tr": "Arayüz", "items": ui})

# ── Rust crates ─────────────────────────────────────────────────────────
core = []
for c in rl["crates"]:
    ts = [tid(f"{c['name']} — {f}", t) for f, t in sorted(c["files"].items())]
    note = note_tr = ""
    if not ts:
        k = first_spdx(c["license"]) or "MIT"
        ts = [spdx(k)]
        note = "The package ships no license file; the standard text of its declared license is included."
        note_tr = "Paket ayrı lisans dosyası içermiyor; beyan edilen lisansın standart metni eklendi."
    core.append({"name": c["name"], "version": c["version"], "license": c["license"],
                 "url": c["repository"], "note": note, "note_tr": note_tr, "texts": ts})
groups.append({"id": "core", "title": "Application core (Rust)", "title_tr": "Uygulama çekirdeği (Rust)", "items": core})

ACK = ["Portions of this software are copyright © The FreeType Project (www.freetype.org). All rights reserved.",
       "This software is based in part on the work of the Independent JPEG Group.",
       "This software includes ICU and Unicode data published by Unicode, Inc."]
ACK_TR = ACK[:2] + ["Bu yazılım Unicode, Inc. tarafından yayımlanan ICU ve Unicode verilerini içerir."]
data = {"generated": datetime.date.today().isoformat(), "acknowledgements": ACK, "acknowledgements_tr": ACK_TR,
        "groups": groups, "texts": texts}
os.makedirs(OUT, exist_ok=True)
json.dump(data, open(os.path.join(OUT, "third-party.json"), "w", encoding="utf-8"), ensure_ascii=False, separators=(",", ":"))

with open(os.path.join(OUT, "THIRD_PARTY_NOTICES.txt"), "w", encoding="utf-8", newline="\r\n") as w:
    w.write("ZORIA ARŞİV — THIRD-PARTY COMPONENTS AND THEIR LICENSES\n")
    w.write("Zoria Arşiv includes the open-source components below. Each is covered by its own license.\n")
    w.write("LGPL/GPL components are distributed unmodified as separate DLL files and are not linked\n"
            "into Zoria Arşiv code; their source code is at the addresses given and is also available\n"
            "from ZoriaSoft on request.\n\n")
    for a in ACK: w.write(a + "\n")
    for g in groups:
        w.write("\n" + "=" * 78 + "\n" + g["title"].upper() + "\n" + "=" * 78 + "\n")
        for it in g["items"]:
            w.write(f"\n- {it['name']} {it['version']}".rstrip() + f"  [{it['license']}]\n")
            if it.get("url"): w.write(f"  {it['url']}\n")
            if it.get("note"): w.write(f"  {it['note']}\n")
            w.write("  License text: " + ", ".join("#" + t for t in it["texts"]) + "\n")
    w.write("\n" + "=" * 78 + "\nLICENSE TEXTS\n" + "=" * 78 + "\n")
    for k, v in texts.items():
        w.write(f"\n----- #{k} · {v['title']} -----\n\n{v['text']}\n")
n = sum(len(g["items"]) for g in groups)
print(n, "components,", len(texts), "unique texts")
