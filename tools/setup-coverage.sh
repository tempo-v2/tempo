#!/bin/sh
set -eu
runtime="${1:-$HOME/Library/Application Support/Tempo Lean/Maia3}"
mkdir -p "$runtime/weights"
weights="$runtime/weights/maia3-5m.pt"
digest=ba14208b2992d85502f5fb501934abf6aaaeb355e9f3fdf90e326911f562524f
verify_weights() {
    shasum -a 256 "$1" | awk '{print $1}' | grep -qx "$digest"
}
if [ ! -f "$weights" ] || ! verify_weights "$weights"; then
    candidate=$(mktemp "$runtime/weights/maia3-5m.XXXXXXXX")
    trap 'rm -f "$candidate"' EXIT
    curl --fail --location --max-time 120 --output "$candidate" \
        'https://huggingface.co/UofTCSSLab/Maia3-5M/resolve/b6559de2398d7140b985f28fd2c19fb5e47ddabe/maia3-5m.pt'
    verify_weights "$candidate"
    mv "$candidate" "$weights"
fi
uv venv --allow-existing --python 3.12 "$runtime/venv"
uv pip install --python "$runtime/venv/bin/python" \
    'maia3 @ git+https://github.com/CSSLab/maia3.git@1e13597c42d4858b7cfd7cfdae01e297263364b2' \
    'torch==2.8.0' 'chess==1.11.2'
