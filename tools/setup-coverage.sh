#!/bin/sh
set -eu
if [ "$(uname -s):$(uname -m)" != Darwin:arm64 ]; then
    echo "Maia provisioning targets macOS on Apple silicon." >&2
    exit 1
fi
project="$(cd "$(dirname "$0")/python" && pwd)"
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
runtime="$(cd "$runtime" && pwd)"
UV_PROJECT_ENVIRONMENT="$runtime/venv" \
    uv sync --project "$project" --python 3.12.12 --locked --extra maia
