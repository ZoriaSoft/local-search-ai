#!/usr/bin/env bash
# Downloads the semantic-search pieces into resources/ — run once per build
# machine (the files are gitignored; too big for git, must not enter it).
# Every download is verified against a pinned sha256.
#
#   bash scripts/fetch-semantic-model.sh            # current platform's ORT
#   bash scripts/fetch-semantic-model.sh windows    # Windows target
#   bash scripts/fetch-semantic-model.sh linux      # Linux target
#
# Model: onnx-community/embeddinggemma-2-ONNX (Apache-2.0), the text
# encoder only (270M). We ship the `q4` export: int4 weights, ~175 MB,
# measured ~37 ms/query on one CPU core vs ~140 ms int8.
# Runtime: ONNX Runtime 1.31.0 (matches ort crate 2.0.0-rc.14's API level).
set -euo pipefail
cd "$(dirname "$0")/.."

REV=daa72c51243991dfcaf9f9137d2c573d8f7790c0
HF="https://huggingface.co/onnx-community/embeddinggemma-2-ONNX/resolve/$REV"
ORT_VER=1.31.0
ORT_DL="https://github.com/microsoft/onnxruntime/releases/download/v$ORT_VER"

die() { echo "ERROR: $*" >&2; exit 1; }

fetch() { # url sha256 dest
    local url=$1 sha=$2 dest=$3
    mkdir -p "$(dirname "$dest")"
    if [ -f "$dest" ] && echo "$sha  $dest" | sha256sum -c --status - 2>/dev/null; then
        echo "ok (cached) $dest"
        return 0
    fi
    echo "fetch $(basename "$dest")"
    curl -fsSL --retry 3 -o "$dest.tmp" "$url" || die "download failed: $url"
    echo "$sha  $dest.tmp" | sha256sum -c --status - || { rm -f "$dest.tmp"; die "sha256 mismatch: $dest"; }
    mv "$dest.tmp" "$dest"
    echo "ok $dest"
}

# Runtime libraries ship inside release archives: fetch + verify the
# archive, then copy out just the library file.
fetch_member() { # archive_url archive_sha member dest
    local url=$1 sha=$2 member=$3 dest=$4
    local tmp
    tmp=$(mktemp -d)
    fetch "$url" "$sha" "$tmp/pkg"
    case "$url" in
        *.zip) (cd "$tmp" && unzip -oq pkg "$member") || die "unzip failed" ;;
        *.tgz) (cd "$tmp" && tar xzf pkg "$member") || die "untar failed" ;;
    esac
    mkdir -p "$(dirname "$dest")"
    cp "$tmp/$member" "$dest" || die "member missing in archive: $member"
    rm -rf "$tmp"
    echo "ok $dest"
}

# ── Model (every platform): text encoder q4 + tokenizer ────────────────
fetch "$HF/onnx/model_q4.onnx"      "f9eeba97acddf139b8ee2ddf04bc30dceafa88de93fadf74d7644e0d61a477a9" resources/models/model_q4.onnx
fetch "$HF/onnx/model_q4.onnx_data" "c3975f2d1ab7a1878ae31a7d7a9b7804a827aff3800b60dfceafce21cac3df49" resources/models/model_q4.onnx_data
fetch "$HF/tokenizer.json"          "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4" resources/models/tokenizer.json

# ── ONNX Runtime library for the requested target ──────────────────────
linux_ort() {
    fetch_member "$ORT_DL/onnxruntime-linux-x64-$ORT_VER.tgz" \
        "cc5c72baf5ae5c8238a6841f0897227be2d02826b9cf98eaf02fdefaeeb03a57" \
        "onnxruntime-linux-x64-$ORT_VER/lib/libonnxruntime.so.$ORT_VER" \
        resources/ort/libonnxruntime.so
}
mac_ort() {
    fetch_member "$ORT_DL/onnxruntime-osx-universal2-$ORT_VER.tgz" \
        "0019dfc4b32d63c1392aa264aed2253c1e0c2fb09216f8e2cc269bbfb8bb49b5" \
        "onnxruntime-osx-universal2-$ORT_VER/lib/libonnxruntime.$ORT_VER.dylib" \
        resources/ort/libonnxruntime.dylib
}
win_ort() {
    fetch_member "$ORT_DL/onnxruntime-win-x64-$ORT_VER.zip" \
        "3958d8a44984160692c1b4ffd0de3cb48f8c39f9d34bb31e433db06e30b82195" \
        "onnxruntime-win-x64-$ORT_VER/lib/onnxruntime.dll" \
        resources/ort/onnxruntime.dll
}

case "${1:-auto}" in
    auto)
        case "$(uname -s)" in
            Linux)  linux_ort ;;
            Darwin) mac_ort ;;
            *)      die "unsupported host; pass linux|windows" ;;
        esac
        ;;
    linux)   linux_ort ;;
    mac|darwin) mac_ort ;;
    windows) win_ort ;;
    *)       die "usage: $0 [auto|linux|mac|windows]" ;;
esac

echo "done — resources/models + resources/ort are ready (gitignored)"
